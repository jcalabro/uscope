//! Running a recording: starting a program under its events, or attaching
//! them to one that runs, then draining the events into a capture until the
//! program ends or the recording is stopped.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::budget::{Budget, Controller, Observation, STEPS};
use super::calls::{self, Calls};
use super::collector::{Collector, ProbeSelection};
use super::record::{self, Sites};
use super::tasks::{Snapshot, Snapshotter};
use super::{
    DEEP_STACK_COPY, Profile, RecordError, RecordOptions, SMALL_STACK_COPY, SampleSource, StackCopy,
};
use crate::backend::perf;
use crate::capture::records::{GapReason, MarkerKind};

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
            save: None,
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
    /// Saves the profile so far when triggered, as the recording goes on.
    pub save: Option<Save<'a>>,
}

/// A trigger, and what takes the profile when it fires: with a rolling
/// window, the window as it is.
pub struct Save<'a> {
    /// Fires once set, and is cleared as the profile is taken.
    pub trigger: &'a AtomicBool,
    pub to: &'a mut dyn FnMut(Profile),
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
        switches: options.off_cpu,
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
    let started = executed.recv().is_ok().then_some(pid);
    drive(
        &mut processor,
        &mut events,
        watch,
        options,
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
        switches: options.off_cpu,
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
        options,
        Some(pid),
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
        ("off-CPU", if options.off_cpu { "on" } else { "off" }.into()),
        (
            "window",
            options.window.map_or_else(
                || "all".into(),
                |window| format!("the last {:.3} s", window.as_secs_f64()),
            ),
        ),
        (
            "task snapshots",
            options.task_snapshots.map_or_else(
                || "off".into(),
                |every| format!("every {:.3} s", every.as_secs_f64()),
            ),
        ),
        ("probes", options.probes.to_string()),
        (
            "budget",
            options.budget.map_or_else(
                || "none".into(),
                |budget| format!("{}% of a CPU", budget.cpu * 100.0),
            ),
        ),
    ] {
        builder.setting(key, &value);
    }
}

/// Drains `events` into `processor` until `ended`, publishing and saving
/// as `watch` asks, and holding to the budget, window, and snapshots of
/// `options`, once the program `started` names has started. When `watch`
/// stops the recording, `stop` ends it at once when it says so, or else
/// waits for `ended`.
fn drive(
    processor: &mut record::Processor,
    events: &mut perf::Events,
    watch: &mut Watch<'_>,
    options: &RecordOptions,
    started: Option<u32>,
    mut ended: impl FnMut() -> bool,
    mut stop: impl FnMut() -> bool,
) {
    let mut pacer = options
        .budget
        .filter(|_| started.is_some())
        .map(|budget| Pacer::new(budget, monotonic(), processor));
    let mut snapshots = started
        .zip(options.task_snapshots)
        .map(|(pid, every)| Snapshots::new(pid, every, options));
    let mut probes = started
        .filter(|_| options.probes != ProbeSelection::Off)
        .map(|pid| Collector::new(pid, options.probes.clone(), options.probe_timeout));
    let started = started.is_some();
    // A window moves by eighths of itself.
    let marks = options.window.map(|window| {
        processor.keep_window(window);
        (window / 8).max(Duration::from_millis(10))
    });
    let mut marked = std::time::Instant::now();
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
            let mut wait = Duration::from_millis(10);
            if let Some(pacer) = &mut pacer {
                wait = wait.min(pacer.pace(monotonic(), events, processor));
            }
            events.wait(wait);
        }
        buffer.clear();
        events.drain(&mut buffer);
        record!(
            "record: drained {} bytes, finished {finished}",
            buffer.len()
        );
        processor.ingest(&buffer, (!finished).then_some(watermark));
        if let Some(probes) = &mut probes {
            if finished {
                probes.finish(processor, !ended());
            } else {
                probes.drain(processor);
            }
        }
        if finished {
            break;
        }
        if let Some(snapshots) = &mut snapshots {
            let sampling = pacer.as_ref().is_none_or(Pacer::sampling);
            snapshots.when_due(processor, events, sampling);
        }
        if let Some(every) = marks
            && marked.elapsed() >= every
        {
            processor.counts(events.counts());
            processor.mark();
            marked = std::time::Instant::now();
        }
        // A program's trigger saves the window as uscope's does.
        let triggered = processor.take_triggered();
        if let Some(save) = &mut watch.save
            && (save.trigger.swap(false, Ordering::Relaxed) || triggered)
        {
            processor.counts(events.counts());
            record!("record: triggered");
            (save.to)(Profile {
                capture: processor.snapshot(),
            });
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
    events.enable(false);
    if let Some(pacer) = pacer {
        pacer.finish(monotonic(), processor);
    }
    processor.counts(events.counts());
}

/// Takes a task snapshot every so often, once the program's debug
/// information is read, with every event off while it holds the program
/// stopped.
struct Snapshots {
    pid: u32,
    every: Duration,
    loading: Option<std::thread::JoinHandle<Result<Snapshotter, RecordError>>>,
    snapshotter: Option<Snapshotter>,
    next: std::time::Instant,
    /// When the last snapshot, or the recording, began.
    previous: u64,
}

impl Snapshots {
    fn new(pid: u32, every: Duration, options: &RecordOptions) -> Self {
        let debug_files = crate::DebugFileOptions {
            directories: options.debug_directories.clone(),
            ..crate::DebugFileOptions::default()
        };
        // Read beside the recording, which drains on meanwhile.
        let program = PathBuf::from(format!("/proc/{pid}/exe"));
        let loading = std::thread::Builder::new()
            .name("uscope-snapshot-load".into())
            .spawn(move || Snapshotter::new(pid, &program, &debug_files))
            .ok();
        Self {
            pid,
            every,
            loading,
            snapshotter: None,
            next: std::time::Instant::now() + every,
            previous: monotonic(),
        }
    }

    /// Takes a snapshot when one is due, leaving the events as `sampling`
    /// says they were.
    fn when_due(
        &mut self,
        processor: &mut record::Processor,
        events: &mut perf::Events,
        sampling: bool,
    ) {
        if std::time::Instant::now() < self.next {
            return;
        }
        if let Some(loading) = self.loading.take_if(|loading| loading.is_finished()) {
            match loading.join() {
                Ok(Ok(snapshotter)) => self.snapshotter = Some(snapshotter),
                Ok(Err(error)) => {
                    processor.snapshot_failed(
                        monotonic(),
                        &format!("task snapshots cannot read the program: {error}"),
                    );
                }
                Err(_) => processor.snapshot_failed(
                    monotonic(),
                    "task snapshots cannot read the program: reading it panicked",
                ),
            }
        }
        let Some(snapshotter) = &self.snapshotter else {
            return;
        };
        if sampling {
            events.enable(false);
        }
        let start = monotonic();
        let taken = snapshotter.take();
        let end = monotonic();
        // With the events off, every record from before the pause is in
        // the rings, and the snapshot claims each sample before it.
        let mut records = Vec::new();
        events.drain(&mut records);
        processor.ingest(&records, Some(start));
        if sampling {
            events.enable(true);
        }
        self.next = std::time::Instant::now() + self.every;
        match taken {
            Ok(snapshot) => {
                let weight = start.saturating_sub(self.previous);
                processor.task_snapshot(self.pid, (start, end), weight, &snapshot);
                self.previous = start;
            }
            Err(error) => {
                processor.task_snapshot(self.pid, (start, end), 0, &Snapshot::default());
                processor.snapshot_failed(start, &format!("a task snapshot failed: {error}"));
            }
        }
    }
}

/// Samples a share of each budget period, in one span at a random offset
/// so that it cannot keep step with a periodic program, and asks the
/// controller once a period how large the next share is.
struct Pacer {
    budget: Budget,
    controller: Controller,
    /// When the period began, and the span of it to sample.
    period: u64,
    window: (u64, u64),
    /// Since when sampling has been on, or off.
    on_since: Option<u64>,
    paused_since: Option<u64>,
    /// What the period has sampled and cost so far.
    sampled: u64,
    cpu: Duration,
    loading: Duration,
    random: u64,
}

impl Pacer {
    fn new(budget: Budget, now: u64, processor: &record::Processor) -> Self {
        Self {
            budget,
            controller: Controller::default(),
            period: now,
            window: (now, u64::MAX),
            on_since: Some(now),
            paused_since: None,
            sampled: 0,
            cpu: record::thread_cpu(),
            loading: processor.loading(),
            random: now | 1,
        }
    }

    /// Whether sampling is on.
    const fn sampling(&self) -> bool {
        self.on_since.is_some()
    }

    /// Turns sampling on or off as the schedule says at `now`, and says
    /// how long until it next changes.
    fn pace(
        &mut self,
        now: u64,
        events: &perf::Events,
        processor: &mut record::Processor,
    ) -> Duration {
        let length = u64::try_from(self.budget.period.as_nanos())
            .unwrap_or(u64::MAX)
            .max(1);
        if now >= self.period.saturating_add(length) {
            self.next_period(now, length, processor);
        }
        let (from, until) = self.window;
        let wanted = from <= now && now < until;
        if wanted && let Some(paused) = self.paused_since.take() {
            processor.gap(paused, monotonic(), GapReason::Budget);
            events.enable(true);
            self.on_since = Some(monotonic());
        } else if !wanted && let Some(on) = self.on_since.take() {
            events.enable(false);
            let off = monotonic();
            self.sampled += off.saturating_sub(on);
            self.paused_since = Some(off);
        }
        let next = [from, until, self.period.saturating_add(length)]
            .into_iter()
            .find(|edge| *edge > now)
            .unwrap_or(now);
        Duration::from_nanos(next - now)
    }

    /// Ends the period at `now`, lets the controller choose the share of
    /// the next one, and places its window.
    fn next_period(&mut self, now: u64, length: u64, processor: &mut record::Processor) {
        if let Some(on) = &mut self.on_since {
            self.sampled += now.saturating_sub(*on);
            *on = now;
        }
        let cpu = record::thread_cpu();
        let loading = processor.loading();
        let observed = Observation {
            cpu: cpu
                .saturating_sub(self.cpu)
                .saturating_sub(loading.saturating_sub(self.loading)),
            sampled: Duration::from_nanos(self.sampled),
            period: Duration::from_nanos(now.saturating_sub(self.period)),
        };
        (self.cpu, self.loading, self.sampled, self.period) = (cpu, loading, 0, now);
        if let Some(decision) = self.controller.step(self.budget, observed) {
            let text = format!(
                "sampling {:.1}% of each period: recording used {:.2}% of a CPU, against a budget of {:.2}%",
                f64::from(decision.steps) * 100.0 / f64::from(STEPS),
                decision.used * 100.0,
                self.budget.cpu * 100.0
            );
            record!("record: budget: {text}");
            processor
                .builder()
                .marker(now.cast_signed(), MarkerKind::Budget, &text);
        }
        let span = length / u64::from(STEPS) * u64::from(self.controller.steps());
        // xorshift: a period's offset needs only to be unpredictable.
        self.random ^= self.random << 13;
        self.random ^= self.random >> 7;
        self.random ^= self.random << 17;
        let offset = self.random % (length - span + 1);
        self.window = (now + offset, now + offset + span);
    }

    /// Ends the recording at `now`: a pause it ends in is a gap to the end.
    fn finish(self, now: u64, processor: &mut record::Processor) {
        if let Some(paused) = self.paused_since {
            processor.gap(paused, now, GapReason::Budget);
        }
    }
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
