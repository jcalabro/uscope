//! A sampling profiler: records where a program's threads spend their
//! time, from outside the program, into a capture that can be saved,
//! loaded, and queried.

mod record;

pub use crate::analysis::{CallTree, Cost, Costs, Granularity, KeyKind, Keys, Query, Transform};

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::backend::perf;
use crate::capture::records::{FrameKind, NONE, Truncation};
use crate::capture::{Capture, file};

/// How much a recording costs the program, and how much it sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preset {
    /// For production: 49 samples a second and a small stack copy.
    Production,
    /// For development: 999 samples a second and a stack copy deep enough
    /// to unwind code without frame pointers.
    #[default]
    Development,
    /// The most the kernel allows.
    Maximum,
}

impl Preset {
    /// Samples per second of CPU time, per thread.
    #[must_use]
    pub const fn rate(self) -> u32 {
        match self {
            Self::Production => 49,
            Self::Development => 999,
            Self::Maximum => 9999,
        }
    }

    /// Bytes of stack each sample copies.
    #[must_use]
    pub const fn stack_bytes(self) -> u32 {
        match self {
            Self::Production => 512,
            Self::Development | Self::Maximum => 16 * 1024,
        }
    }
}

/// What makes a sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleSource {
    /// CPU time, at `rate` samples a second.
    Clock { rate: u32 },
    /// Each execution of the first instruction of the function a symbol of
    /// the program names. The program runs without address randomization.
    Function { symbol: String },
}

impl SampleSource {
    /// What a sample weighs: nanoseconds of CPU time, or one execution.
    #[must_use]
    pub const fn unit(&self) -> &'static str {
        match self {
            Self::Clock { .. } => "nanoseconds",
            Self::Function { .. } => "executions",
        }
    }
}

impl std::fmt::Display for SampleSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Clock { rate } => write!(f, "CPU time at {rate} Hz"),
            Self::Function { symbol } => write!(f, "each call of {symbol}"),
        }
    }
}

/// How to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordOptions {
    pub source: SampleSource,
    pub stack_bytes: u32,
    /// The most frames a stack keeps.
    pub max_frames: usize,
    /// Directories searched for separate debug files.
    pub debug_directories: Vec<PathBuf>,
}

impl RecordOptions {
    #[must_use]
    pub const fn preset(preset: Preset) -> Self {
        Self {
            source: SampleSource::Clock {
                rate: preset.rate(),
            },
            stack_bytes: preset.stack_bytes(),
            max_frames: 512,
            debug_directories: Vec::new(),
        }
    }
}

impl Default for RecordOptions {
    fn default() -> Self {
        Self::preset(Preset::default())
    }
}

/// Why a recording failed.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error(transparent)]
    Launch(#[from] perf::LaunchError),
    #[error("starting the program failed: {0}")]
    Start(std::io::Error),
    #[error("the program has no function named '{0}'")]
    NoSuchFunction(String),
    #[error("{count} functions are named '{name}'; name one by its symbol")]
    AmbiguousFunction { name: String, count: usize },
    #[error("reading the program failed: {0}")]
    Program(String),
    #[error(
        "the program was not loaded where the breakpoint assumed (bias {expected:#x}, not {actual:#x}): its samples would be of another instruction"
    )]
    Misplaced { expected: u64, actual: u64 },
}

/// A recorded profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    capture: Capture,
}

/// A recording and how its program ended.
#[derive(Debug)]
pub struct Recording {
    pub profile: Profile,
    pub status: ExitStatus,
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
    /// Ends the recording once set, by killing the program.
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
    let (source, breakpoint) = perf_source(&program, &options.source)?;
    let events = perf::EventOptions {
        source,
        stack_bytes: options.stack_bytes.next_multiple_of(8),
        ring_pages: 128,
    };
    let launch = perf::LaunchOptions {
        no_address_randomization: breakpoint.is_some(),
    };
    let perf::Launched {
        pid,
        events,
        exit,
        killer,
        executed,
    } = perf::launch(command, &events, launch)?;
    let search = crate::debug_info::DebugFileSearch::new(&crate::DebugFileOptions {
        directories: options.debug_directories.clone(),
        ..crate::DebugFileOptions::default()
    });
    let ring_pages = events.pages();
    let mut processor = record::Processor::new(search, options.max_frames);
    let builder = processor.builder();
    for (key, value) in [
        ("program", program.to_string_lossy().into_owned()),
        ("pid", pid.to_string()),
        ("ring pages", ring_pages.to_string()),
        ("source", options.source.to_string()),
        ("weight", options.source.unit().to_owned()),
        ("stack bytes", options.stack_bytes.to_string()),
    ] {
        builder.setting(key, &value);
    }
    // A program that failed to execute ends the recording with why, below,
    // without having been published as started.
    let started = executed.recv().is_ok();
    if started {
        (watch.publish)(Profile {
            capture: processor.snapshot(),
        });
    }
    let mut published = std::time::Instant::now();
    let mut wait = watch.every.min(Duration::from_millis(250));
    let mut killed = false;
    let mut events = events;
    let mut buffer = Vec::new();
    loop {
        if !killed && watch.stop.load(Ordering::Relaxed) {
            record!("record: stopping, killing {pid}");
            killer.kill();
            killed = true;
        }
        let finished = exit.is_finished();
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
            (watch.publish)(Profile {
                capture: processor.snapshot(),
            });
            published = std::time::Instant::now();
            wait = (wait * 2).min(watch.every);
        }
    }
    events.disable();
    let status = exit
        .join()
        .unwrap_or_else(|_| Err(std::io::Error::other("the program's waiter panicked")))
        .map_err(RecordError::Start)?;
    record!("record: the program ended: {status}");
    let capture = processor.finish();
    let profile = Profile { capture };
    if let Some(expected) = breakpoint {
        profile.check_bias(&program, expected)?;
    }
    Ok(Recording { profile, status })
}

/// The event that takes `source`'s samples in `program`, and for a
/// breakpoint, the bias the program must load at for it to be right.
fn perf_source(
    program: &Path,
    source: &SampleSource,
) -> Result<(perf::Source, Option<u64>), RecordError> {
    Ok(match source {
        SampleSource::Clock { rate } => (
            perf::Source::Clock {
                period: Duration::from_nanos(1_000_000_000 / u64::from((*rate).max(1))),
            },
            None,
        ),
        SampleSource::Function { symbol } => {
            let (address, bias) = function_address(program, symbol)?;
            (
                perf::Source::Breakpoint {
                    address: address + bias,
                },
                Some(bias),
            )
        }
    })
}

/// How far behind a drain records are processed.
const WATERMARK_DELAY: u64 = 20_000_000;

fn monotonic() -> u64 {
    nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).map_or(0, |time| {
        u64::try_from(time.tv_sec()).unwrap_or(0) * 1_000_000_000
            + u64::try_from(time.tv_nsec()).unwrap_or(0)
    })
}

/// The query of every sample, seen as `view` shows stacks.
fn query(view: StackView) -> Query {
    Query {
        logical: view == StackView::Logical,
        ..Query::default()
    }
}

/// The image address of the function `symbol` names in `program`, and the
/// bias the program loads at without address randomization.
fn function_address(program: &Path, symbol: &str) -> Result<(u64, u64), RecordError> {
    /// Where Linux loads a position-independent executable when it does
    /// not randomize: two thirds of a 47-bit address space, page aligned.
    const UNRANDOMIZED_PIE_BASE: u64 = 0x5555_5555_4000;
    let program = which(program).ok_or_else(|| RecordError::Program("not found".into()))?;
    let data = std::fs::read(&program).map_err(|error| RecordError::Program(error.to_string()))?;
    let file = object::File::parse(data.as_slice())
        .map_err(|error| RecordError::Program(error.to_string()))?;
    let bias = match object::Object::kind(&file) {
        object::ObjectKind::Dynamic => UNRANDOMIZED_PIE_BASE,
        _ => 0,
    };
    // A symbol, or the name a profile shows for one.
    let addresses: BTreeSet<u64> = object::Object::symbols(&file)
        .filter(|candidate| {
            object::ObjectSymbol::kind(candidate) == object::SymbolKind::Text
                && object::ObjectSymbol::name(candidate).is_ok_and(|name| {
                    name == symbol
                        || crate::demangle::qualified_name(name).is_some_and(|name| name == symbol)
                })
        })
        .map(|found| object::ObjectSymbol::address(&found))
        .collect();
    let mut addresses = addresses.into_iter();
    match (addresses.next(), addresses.len()) {
        (None, _) => Err(RecordError::NoSuchFunction(symbol.to_owned())),
        (Some(address), 0) => Ok((address, bias)),
        (Some(_), others) => Err(RecordError::AmbiguousFunction {
            name: symbol.to_owned(),
            count: others + 1,
        }),
    }
}

/// The file a command names, as `execvp` would find it.
fn which(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return Some(program.to_owned());
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(program))
            .find(|candidate| candidate.is_file())
    })
}

/// How a stack ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackEnding {
    Complete,
    /// Short of its outermost frame, for this reason.
    Truncated(String),
}

/// One sample's stack, as names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampledStack {
    pub pid: u32,
    pub tid: u32,
    pub time: i64,
    pub weight: u64,
    /// Function names, innermost first, each inlined function a frame of
    /// its own; a frame without symbols is its module's address.
    pub functions: Vec<String>,
    pub ending: StackEnding,
}

/// What a function costs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionCost {
    pub name: String,
    /// The weight of samples it was the innermost function of.
    pub own: u64,
    /// The weight of samples it was anywhere on the stack of.
    pub total: u64,
}

/// How a profile's stacks are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StackView {
    /// Each async task's frames as a stack of their own, rooted under the
    /// runtime's dispatch, with wrappers folded away.
    #[default]
    Logical,
    /// The frames as the thread ran them.
    Native,
}

/// A thread a profile sampled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileThread {
    /// The thread's row, by which a [`Query`] selects it.
    pub row: u32,
    pub pid: u32,
    pub tid: u32,
    /// The name it last had, if the kernel reported one.
    pub name: Option<String>,
    pub samples: u64,
    pub weight: u64,
}

/// A function called from its parent's node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallNode {
    pub parent: Option<usize>,
    pub name: String,
    pub own: u64,
    pub total: u64,
}

/// What reading a profile file found.
#[derive(Debug)]
pub struct ReadProfile {
    pub profile: Profile,
    /// Whether the file was cut short, as by a crash, and only its complete
    /// parts were read.
    pub truncated: bool,
    /// The parts read.
    pub chunks: usize,
}

impl Profile {
    /// Reads a profile file.
    pub fn read(path: &Path) -> Result<ReadProfile, ProfileFileError> {
        Self::from_bytes(&std::fs::read(path)?)
    }

    /// Whether `bytes` begin as a profile file does.
    #[must_use]
    pub fn is_profile(bytes: &[u8]) -> bool {
        bytes.starts_with(&file::FILE_MAGIC)
    }

    /// Reads the bytes of a profile file.
    pub fn from_bytes(bytes: &[u8]) -> Result<ReadProfile, ProfileFileError> {
        let read =
            file::read(bytes).map_err(|error| ProfileFileError::Invalid(error.to_string()))?;
        Ok(ReadProfile {
            profile: Self {
                capture: read.capture,
            },
            truncated: read.truncated,
            chunks: read.chunks,
        })
    }

    /// The profile as a file holds it.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        file::write(&self.capture, &mut bytes).expect("writing to memory cannot fail");
        bytes
    }

    /// Writes the profile to a file.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let out = std::io::BufWriter::new(std::fs::File::create(path)?);
        file::write(&self.capture, out)
    }

    #[must_use]
    pub const fn samples(&self) -> usize {
        self.capture.samples.len()
    }

    /// A setting the recording recorded, such as `program`.
    #[must_use]
    pub fn setting(&self, key: &str) -> Option<&str> {
        self.capture.setting(key)
    }

    /// Every setting the recording recorded, by key.
    #[must_use]
    pub fn settings(&self) -> Vec<(&str, &str)> {
        let capture = &self.capture;
        capture
            .settings
            .iter()
            .map(|setting| {
                (
                    capture.string(setting.key.get()),
                    capture.string(setting.value.get()),
                )
            })
            .collect()
    }

    /// The first and last times the recording saw, in nanoseconds of
    /// `CLOCK_MONOTONIC`.
    #[must_use]
    pub fn span(&self) -> (i64, i64) {
        self.capture.span()
    }

    /// The weight of every sample: nanoseconds of CPU time for a clock
    /// source, and executions for a function source.
    #[must_use]
    pub fn total_weight(&self) -> u64 {
        self.capture
            .samples
            .iter()
            .map(|sample| sample.weight.get())
            .sum()
    }

    /// The keys frames stand for at `granularity`, through which every
    /// query of the profile runs.
    #[must_use]
    pub fn keys(&self, granularity: Granularity) -> Keys<'_> {
        Keys::new(&self.capture, granularity)
    }

    /// Each thread that was sampled, by its row, which queries name.
    #[must_use]
    pub fn threads(&self) -> Vec<ProfileThread> {
        let capture = &self.capture;
        capture
            .sample_ranges
            .iter()
            .map(|range| {
                let row = range.thread.get();
                let thread = &capture.threads[row as usize];
                let samples = capture.thread_samples(range);
                ProfileThread {
                    row,
                    pid: capture.processes[thread.process.get() as usize].pid.get(),
                    tid: thread.tid.get(),
                    name: Some(capture.string(thread.name.get()))
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned),
                    samples: samples.len() as u64,
                    weight: samples.iter().map(|sample| sample.weight.get()).sum(),
                }
            })
            .collect()
    }

    /// How many times the kernel throttled sampling.
    #[must_use]
    pub fn throttles(&self) -> u64 {
        self.capture
            .account(crate::capture::records::AccountKind::Throttles as u32)
    }

    /// What each function costs, most expensive first.
    #[must_use]
    pub fn top(&self, view: StackView) -> Vec<FunctionCost> {
        let keys = self.keys(Granularity::Function);
        keys.functions(&query(view))
            .rows
            .into_iter()
            .map(|cost| FunctionCost {
                name: keys.name(cost.key),
                own: cost.own,
                total: cost.total,
            })
            .collect()
    }

    /// The call tree, from the outermost callers in; parents come before
    /// their children.
    #[must_use]
    pub fn call_tree(&self, view: StackView) -> Vec<CallNode> {
        let keys = self.keys(Granularity::Function);
        let tree = keys.aggregate(&query(view));
        (0..tree.len())
            .map(|node| CallNode {
                parent: (tree.parent[node] != NONE).then_some(tree.parent[node] as usize),
                name: keys.name(tree.key[node]),
                own: tree.own[node],
                total: tree.total[node],
            })
            .collect()
    }

    /// Samples the kernel dropped.
    #[must_use]
    pub fn lost_samples(&self) -> u64 {
        self.capture
            .account(crate::capture::records::AccountKind::LostSamples as u32)
    }

    /// How many stacks ended short of their outermost frame, by reason.
    #[must_use]
    pub fn truncations(&self) -> Vec<(String, u64)> {
        record::truncation_counts(&self.capture)
            .into_iter()
            .map(|(reason, count)| (reason.describe().to_owned(), count))
            .collect()
    }

    /// Every sample's stack, by thread and then time.
    pub fn stacks(&self) -> Vec<SampledStack> {
        let capture = &self.capture;
        let mut stacks = Vec::with_capacity(capture.samples.len());
        for range in &capture.sample_ranges {
            let thread = &capture.threads[range.thread.get() as usize];
            let tid = thread.tid.get();
            let pid = capture.processes[thread.process.get() as usize].pid.get();
            for sample in capture.thread_samples(range) {
                let mut functions = Vec::new();
                let mut ending = StackEnding::Complete;
                for frame in capture.stack_frames(sample.stack.get()) {
                    let record = &capture.frames[frame as usize];
                    if record.kind == FrameKind::Truncated as u8 {
                        let reason = Truncation::from_number(record.address.get())
                            .map_or("unknown", Truncation::describe);
                        ending = StackEnding::Truncated(reason.to_owned());
                        continue;
                    }
                    let lines = capture.frame_lines(frame);
                    if lines.is_empty() {
                        functions.push(self.address_name(frame));
                    }
                    for line in lines {
                        let function = &capture.functions[line.function.get() as usize];
                        functions.push(capture.string(function.name.get()).to_owned());
                    }
                }
                stacks.push(SampledStack {
                    pid,
                    tid,
                    time: sample.time.get(),
                    weight: sample.weight.get(),
                    functions,
                    ending,
                });
            }
        }
        stacks
    }

    /// A frame without symbols, as `module+0xoffset`.
    fn address_name(&self, frame: u32) -> String {
        let record = &self.capture.frames[frame as usize];
        let module = record.module.get();
        if module == NONE {
            return format!("{:#x}", record.address.get());
        }
        let path = self
            .capture
            .string(self.capture.modules[module as usize].path.get());
        let name = Path::new(path)
            .file_name()
            .map_or_else(|| path.into(), |name| name.to_string_lossy());
        format!("{name}+{:#x}", record.address.get())
    }

    /// Whether every mapping of `program` has the bias a breakpoint
    /// source assumed.
    fn check_bias(&self, program: &Path, expected: u64) -> Result<(), RecordError> {
        let program = which(program).and_then(|program| std::fs::canonicalize(program).ok());
        let capture = &self.capture;
        for mapping in &capture.mappings {
            let path = capture.string(capture.modules[mapping.module.get() as usize].path.get());
            if program.as_deref() == Some(Path::new(path)) && mapping.bias.get() != expected {
                return Err(RecordError::Misplaced {
                    expected,
                    actual: mapping.bias.get(),
                });
            }
        }
        Ok(())
    }
}

/// Why a profile file could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ProfileFileError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The file is not a profile, or is damaged.
    #[error("{0}")]
    Invalid(String),
}
