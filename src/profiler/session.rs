//! Running a recording: starting a program under its events, or attaching
//! them to one that runs, then draining the events into a capture until the
//! program ends or the recording is stopped.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::calls::{self, Calls};
use super::record::{self, Sites};
use super::{
    DEEP_STACK_COPY, Profile, RecordError, RecordOptions, SMALL_STACK_COPY, SampleSource, StackCopy,
};
use crate::backend::perf;

/// A recording and how its program ended.
#[derive(Debug)]
pub struct Recording {
    pub profile: Profile,
    pub status: ExitStatus,
}

/// A recording of a process that was running already.
#[derive(Debug)]
pub struct Attachment {
    pub profile: Profile,
    /// Whether the process exited, rather than the recording being stopped.
    pub exited: bool,
}

/// Runs `command` to its exit, recording it.
pub fn record(command: Command, options: &RecordOptions) -> Result<Recording, RecordError> {
    let stop = AtomicBool::new(false);
    record_watched(
        command,
        options,
        &mut Watch {
            every: Duration::MAX,
            publish: &mut |_| {},
            stop: &stop,
        },
    )
}

/// What a recording does while it runs, for a live view of it.
pub struct Watch<'a> {
    /// How often the profile so far is published, at most: the first
    /// comes within a quarter second, and each wait doubles up to this.
    pub every: Duration,
    /// Takes the profile so far: once when the program has started, when
    /// it holds nothing yet, and every `every` after.
    pub publish: &'a mut dyn FnMut(Profile),
    /// Ends the recording once set: a program the recording started is
    /// killed, and one it attached to goes on without it.
    pub stop: &'a AtomicBool,
}

/// Records `command` as [`record`] does, publishing the profile so far as
/// `watch` asks.
pub fn record_watched(
    command: Command,
    options: &RecordOptions,
    watch: &mut Watch<'_>,
) -> Result<Recording, RecordError> {
    let _span = crate::span!("record");
    let program = PathBuf::from(command.get_program());
    let (source, breakpoint) = perf_source(&program, &options.source, None)?;
    let mut processor = processor(options, breakpoint);
    let (stack_bytes, stack_copy) = stack_copy(&mut processor, &program, options.stack);
    let events = perf::EventOptions {
        source,
        stack_bytes,
        ring_pages: options.ring_pages,
    };
    let launch = perf::LaunchOptions {
        no_address_randomization: breakpoint.is_some(),
    };
    let perf::Launched {
        pid,
        mut events,
        exit,
        killer,
        executed,
    } = perf::launch(command, &events, launch)?;
    settings(
        &mut processor,
        options,
        &program,
        pid,
        events.pages(),
        stack_bytes,
        stack_copy,
    );
    // A program that failed to execute ends the recording with why, below,
    // without having been published as started.
    let started = executed.recv().is_ok();
    drive(
        &mut processor,
        &mut events,
        watch,
        started,
        || exit.is_finished(),
        || {
            record!("record: stopping, killing {pid}");
            killer.kill();
            false
        },
    );
    let status = exit
        .join()
        .unwrap_or_else(|_| Err(std::io::Error::other("the program's waiter panicked")))
        .map_err(RecordError::Start)?;
    record!("record: the program ended: {status}");
    let profile = Profile {
        capture: processor.finish(),
    };
    if let Some(calls) = breakpoint {
        profile.check_bias(&program, calls.bias)?;
    }
    Ok(Recording { profile, status })
}

/// Records the running process `pid`, which goes on as it was.
///
/// The events follow it from outside, and nothing stops it. The recording ends when
/// the process exits or `watch` stops it, and publishes as `watch` asks.
pub fn record_attached(
    pid: u32,
    options: &RecordOptions,
    watch: &mut Watch<'_>,
) -> Result<Attachment, RecordError> {
    let _span = crate::span!("record attached");
    let program = PathBuf::from(format!("/proc/{pid}/exe"));
    let bias = perf::program_bias(pid);
    let (source, breakpoint) = perf_source(&program, &options.source, bias)?;
    let mut processor = processor(options, breakpoint);
    let (stack_bytes, stack_copy) = stack_copy(&mut processor, &program, options.stack);
    let events = perf::EventOptions {
        source,
        stack_bytes,
        ring_pages: options.ring_pages,
    };
    let perf::Attached {
        mut events,
        process,
    } = perf::attach(pid, &events, options.max_events)?;
    processor.attached(pid, monotonic());
    let name = std::fs::read_link(&program).unwrap_or(program);
    let pages = events.pages();
    settings(
        &mut processor,
        options,
        &name,
        pid,
        pages,
        stack_bytes,
        stack_copy,
    );
    let mut stopped = false;
    drive(
        &mut processor,
        &mut events,
        watch,
        true,
        || process.exited(),
        || {
            record!("record: stopping, detaching from {pid}");
            stopped = true;
            true
        },
    );
    record!("record: detached from {pid}, stopped {stopped}");
    Ok(Attachment {
        profile: Profile {
            capture: processor.finish(),
        },
        exited: !stopped,
    })
}

/// A processor for `options`, and for a breakpoint's repeats.
fn processor(options: &RecordOptions, breakpoint: Option<Calls>) -> record::Processor {
    let search = crate::debug_info::DebugFileSearch::new(&crate::DebugFileOptions {
        directories: options.debug_directories.clone(),
        ..crate::DebugFileOptions::default()
    });
    let mut processor = record::Processor::new(search, options.max_frames);
    if let Some(calls) = breakpoint
        && let Some(branch) = calls.repeats_when
    {
        processor.repeats_when(calls.address + calls.bias, branch);
    }
    processor
}

/// Records how the recording was made.
fn settings(
    processor: &mut record::Processor,
    options: &RecordOptions,
    program: &Path,
    pid: u32,
    ring_pages: usize,
    stack_bytes: u32,
    stack_copy: String,
) {
    let builder = processor.builder();
    for (key, value) in [
        ("program", program.to_string_lossy().into_owned()),
        ("pid", pid.to_string()),
        ("ring pages", ring_pages.to_string()),
        ("source", options.source.to_string()),
        ("weight", options.source.unit().to_owned()),
        ("stack bytes", stack_bytes.to_string()),
        ("stack copy", stack_copy),
    ] {
        builder.setting(key, &value);
    }
}

/// Drains `events` into `processor` until `ended`, publishing as `watch`
/// asks once the program has `started`. When `watch` stops the recording,
/// `stop` ends it at once when it says so, or else waits for `ended`.
fn drive(
    processor: &mut record::Processor,
    events: &mut perf::Events,
    watch: &mut Watch<'_>,
    started: bool,
    mut ended: impl FnMut() -> bool,
    mut stop: impl FnMut() -> bool,
) {
    if started {
        (watch.publish)(Profile {
            capture: processor.snapshot(),
        });
    }
    let mut published = std::time::Instant::now();
    let mut wait = watch.every.min(Duration::from_millis(250));
    let mut stopping = false;
    let mut now = false;
    let mut buffer = Vec::new();
    loop {
        if !stopping && watch.stop.load(Ordering::Relaxed) {
            stopping = true;
            now = stop();
        }
        let finished = now || ended();
        // Every record stamped before this has been written by the time
        // the next drain begins, but for a stalled CPU.
        let watermark = monotonic().saturating_sub(WATERMARK_DELAY);
        if !finished {
            events.wait(Duration::from_millis(10));
        }
        buffer.clear();
        events.drain(&mut buffer);
        record!(
            "record: drained {} bytes, finished {finished}",
            buffer.len()
        );
        processor.ingest(&buffer, (!finished).then_some(watermark));
        if finished {
            break;
        }
        if started && published.elapsed() >= wait {
            processor.counts(events.counts());
            (watch.publish)(Profile {
                capture: processor.snapshot(),
            });
            published = std::time::Instant::now();
            wait = (wait * 2).min(watch.every);
        }
    }
    events.disable();
    processor.counts(events.counts());
}

/// The event that takes `source`'s samples in `program`, and for a
/// breakpoint, where it counts calls. A breakpoint's addresses assume the
/// program loads at `bias`, or where it does without address randomization.
fn perf_source(
    program: &Path,
    source: &SampleSource,
    bias: Option<u64>,
) -> Result<(perf::Source, Option<Calls>), RecordError> {
    Ok(match source {
        SampleSource::Clock { rate } => (
            perf::Source::Clock {
                period: Duration::from_nanos(1_000_000_000 / u64::from((*rate).max(1))),
            },
            None,
        ),
        SampleSource::Function { symbol } => {
            let calls = calls::plan(program, symbol, bias)?;
            (
                perf::Source::Breakpoint {
                    address: calls.address + calls.bias,
                },
                Some(calls),
            )
        }
    })
}

/// The bytes of stack each sample of `program` copies, and why.
fn stack_copy(
    processor: &mut record::Processor,
    program: &Path,
    stack: StackCopy,
) -> (u32, String) {
    let bytes = match stack {
        StackCopy::Bytes(bytes) => return (bytes.next_multiple_of(8), "as asked".into()),
        StackCopy::Auto => DEEP_STACK_COPY,
    };
    let Some(path) = which(program).and_then(|path| std::fs::canonicalize(path).ok()) else {
        return (bytes, "auto: the program was not found".into());
    };
    match processor.call_sites(&path) {
        None => (bytes, "auto: the program's code could not be read".into()),
        Some(Sites::Dynamic) => (
            bytes,
            "auto: the program loads libraries, which are known only once it runs".into(),
        ),
        Some(Sites::Static(sites, _)) if sites.keep_the_chain() => (
            SMALL_STACK_COPY,
            "auto: the program keeps the frame-pointer chain where it calls".into(),
        ),
        Some(Sites::Static(_, function)) => (
            bytes,
            format!(
                "auto: {} calls without keeping the frame-pointer chain",
                function.as_deref().unwrap_or("a function")
            ),
        ),
    }
}

/// How far behind a drain records are processed.
const WATERMARK_DELAY: u64 = 20_000_000;

fn monotonic() -> u64 {
    nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).map_or(0, |time| {
        u64::try_from(time.tv_sec()).unwrap_or(0) * 1_000_000_000
            + u64::try_from(time.tv_nsec()).unwrap_or(0)
    })
}

/// The file a command names, as `execvp` would find it.
pub(super) fn which(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return Some(program.to_owned());
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(program))
            .find(|candidate| candidate.is_file())
    })
}
