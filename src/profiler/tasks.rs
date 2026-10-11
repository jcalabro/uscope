//! Task snapshots: where an async program's tasks wait.
//!
//! A task that waits runs no code, so no CPU sample sees it. A snapshot
//! stops the program through the debugger's attach, lists every task of
//! its runtimes, reads each waiting task's stack, and detaches, leaving
//! the program to go on as it was. Each wait then stands for the time
//! since the snapshot before.
//!
//! A live snapshot reads the same from the program's memory while it
//! runs, through its runtimes' models alone. A task may change while it
//! is read, so each is read twice: its context, and every page of its
//! stack or every future of its chain of awaits, must read the same both
//! times, or the task is left out and counted. A live snapshot is then
//! consistent for each task, not across them. A runtime found through a
//! thread's thread-local storage, as tokio is, needs the thread's thread
//! pointer, which only a stop reads: a thread keeps it for life, so a
//! stopped snapshot learns each thread's, and a live snapshot that meets a
//! thread it has not learned gives way to a stopped one.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::RecordError;
use crate::capture::records::{Truncation, role_number};
use crate::runtime_model::{self, RuntimeStop};
use crate::{
    Debugger, DebuggerHandle, ExecutionContext, FrameKind, ModuleId, ModuleImage, ProcessId,
    Program, StackFrameId, StopContext, StopId, TaskSnapshot, TaskState, TypeInfo, TypeReference,
    UnwindTermination,
};

/// The most tasks one snapshot reads.
const MOST_TASKS: usize = 10_000;

/// Where one waiting task waits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    /// What its runtime calls a task, and its number.
    pub noun: &'static str,
    pub number: u64,
    /// The runtime's words for what it waits for, or its state.
    pub state: String,
    /// Its labels, as `key=value` lines.
    pub labels: Option<String>,
    /// Its stack, innermost first.
    pub frames: Vec<WaitFrame>,
}

/// One frame of a waiting task's stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitFrame {
    /// Code, by the address that names its line as a sample's would: a
    /// caller's is one byte before its return address.
    Code { address: u64, role: u8 },
    /// The future a suspended task awaits, by what it waits for.
    Awaited(String),
    /// Where its stack ends early.
    Truncated(Truncation),
}

/// Where a task's runtime keeps it, as a snapshot found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub object: u64,
    pub noun: &'static str,
    pub number: u64,
    pub labels: Option<String>,
}

/// One snapshot's tasks, where each waits, and why it may miss some.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Every task the runtime keeps somewhere, running or not.
    pub located: Vec<Located>,
    pub waiting: Vec<Waiting>,
    /// Each task it could not read, or part of the runtime's tasks, and
    /// why.
    pub problems: Vec<String>,
    /// How many waiting tasks a live snapshot left out because they
    /// changed while it read them.
    pub changed: u64,
}

/// Takes snapshots of one process.
pub struct Snapshotter {
    pid: u32,
    program: Program,
    /// Each thread's thread pointer, by its id and when it started, as
    /// stopped snapshots that learned them read.
    thread_pointers: BTreeMap<(u32, u64), u64>,
}

impl Snapshotter {
    /// Reads the debug information of `program`, which process `pid` runs,
    /// once for every snapshot.
    pub fn new(
        pid: u32,
        program: &Path,
        debug_files: &crate::DebugFileOptions,
    ) -> Result<Self, RecordError> {
        let program = Program::load(program, debug_files)
            .map_err(|error| RecordError::Program(error.to_string()))?;
        Ok(Self {
            pid,
            program,
            thread_pointers: BTreeMap::new(),
        })
    }

    /// Stops the process, reads where its tasks wait, and lets it go on,
    /// learning every thread's thread pointer for live snapshots when
    /// `learn` says to. A thread of its own runs the snapshot in a runtime
    /// of its own, as the caller may run in one.
    pub fn take(&mut self, learn: bool) -> Result<Snapshot, crate::Error> {
        let _span = crate::span!("task snapshot");
        let (snapshot, learned) = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?
                        .block_on(self.read(learn))
                })
                .join()
                .unwrap_or_else(|_| Err(crate::Error::RequestCancelled))
        })?;
        if learn {
            self.thread_pointers.clear();
            for (tid, pointer) in learned {
                if let Some(start) = thread_start(self.pid, tid) {
                    self.thread_pointers.insert((tid, start), pointer);
                }
            }
        }
        Ok(snapshot)
    }

    async fn read(&self, learn: bool) -> Result<(Snapshot, Vec<(u32, u64)>), crate::Error> {
        let debugger = Debugger::for_program(self.program.clone())?;
        let handle = debugger.handle();
        let read = match handle
            .attach_process(ProcessId::new(u64::from(self.pid)))
            .await
        {
            Ok(stop) => match read(&handle, stop).await {
                Ok(snapshot) if learn => thread_pointers(&handle, stop)
                    .await
                    .map(|learned| (snapshot, learned)),
                Ok(snapshot) => Ok((snapshot, Vec::new())),
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        // The process goes on whether or not reading it failed.
        let detached = debugger.shutdown().await;
        let read = read?;
        detached?;
        Ok(read)
    }
}

/// Every thread's thread pointer at `stop`, by its thread id.
async fn thread_pointers(
    handle: &DebuggerHandle,
    stop: StopId,
) -> Result<Vec<(u32, u64)>, crate::Error> {
    let state = handle.snapshot().await?;
    let mut learned = Vec::with_capacity(state.threads.len());
    for thread in state.threads.iter() {
        let context = StopContext {
            stop,
            execution: ExecutionContext::Thread(thread.id),
            frame: StackFrameId::INNERMOST,
        };
        let registers = handle.at(context).registers().await?;
        let pointer = registers
            .registers
            .iter()
            .find(|value| &*value.register.name == "fs_base")
            .and_then(|value| Some(u64::from_le_bytes(value.bytes.as_deref()?.try_into().ok()?)));
        if let (Some(pointer), Ok(tid)) = (pointer, u32::try_from(thread.id.get())) {
            learned.push((tid, pointer));
        }
    }
    Ok(learned)
}

/// When thread `tid` of process `pid` started, in clock ticks since boot,
/// which tells it from a later thread given its id.
fn thread_start(pid: u32, tid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/stat")).ok()?;
    // The name, in parentheses, may hold anything; the fields after it
    // begin with the state, the third.
    let after = &stat[stat.rfind(')')? + 2..];
    after.split(' ').nth(19)?.parse().ok()
}

/// Every waiting task's stack at `stop`.
async fn read(handle: &DebuggerHandle, stop: StopId) -> Result<Snapshot, crate::Error> {
    let mut snapshot = Snapshot::default();
    let mut images = BTreeMap::new();
    let mut cursor = None;
    let mut seen = 0;
    loop {
        let page = handle.tasks(cursor, 256).await?;
        snapshot
            .problems
            .extend(page.gaps.iter().map(ToString::to_string));
        snapshot
            .located
            .extend(page.tasks.iter().filter_map(|task| {
                Some(Located {
                    object: task.object?.get(),
                    noun: task.noun,
                    number: task.id.number,
                    labels: labels(task),
                })
            }));
        for task in page.tasks.iter().filter(|task| waits(task)) {
            let context = StopContext {
                stop,
                execution: ExecutionContext::Task(task.id),
                frame: StackFrameId::INNERMOST,
            };
            match handle.at(context).backtrace().await {
                Ok(backtrace) => {
                    let mut futures = Vec::new();
                    for frame in backtrace.frames.iter() {
                        if let FrameKind::Awaited { ty, .. } = frame.kind {
                            futures.push(future_name(handle, &mut images, frame.module, ty).await);
                        }
                    }
                    snapshot.waiting.push(Waiting {
                        noun: task.noun,
                        number: task.id.number,
                        state: state(task),
                        labels: labels(task),
                        frames: frames(&backtrace, futures),
                    });
                }
                Err(error) => snapshot
                    .problems
                    .push(format!("{} {}: {error}", task.noun, task.id)),
            }
        }
        seen += page.tasks.len();
        match page.next {
            Some(next) if seen < MOST_TASKS => cursor = Some(next),
            Some(_) => {
                snapshot
                    .problems
                    .push(format!("only the first {MOST_TASKS} tasks were read"));
                return Ok(snapshot);
            }
            None => return Ok(snapshot),
        }
    }
}

/// How a backtrace names the future of type `ty` a suspended task awaits,
/// which `module`'s image describes: `awaiting tokio::time::sleep::Sleep`.
/// What it waits for, which changes as it waits, is its task's state.
async fn future_name(
    handle: &DebuggerHandle,
    images: &mut BTreeMap<ModuleId, Option<Arc<ModuleImage>>>,
    module: Option<ModuleId>,
    ty: TypeReference,
) -> String {
    let image = match module {
        Some(module) => {
            if let std::collections::btree_map::Entry::Vacant(entry) = images.entry(module) {
                entry.insert(handle.loaded_module_image(module).await.ok());
            }
            images.get(&module).cloned().flatten()
        }
        None => None,
    };
    let name = image
        .filter(|image| image.id() == ty.image)
        .and_then(|image| image.type_info(ty).map(TypeInfo::qualified_name))
        .unwrap_or_else(|| "a future".to_owned());
    format!("awaiting {name}")
}

/// Whether a task waits: it is on no thread, and is the program's. A task
/// that runs is a CPU sample's to see, and the runtime's own, such as Go's
/// garbage collector's workers, wait for the runtime, as Go's goroutine
/// profile leaves them out.
const fn waits(task: &TaskSnapshot) -> bool {
    !task.internal
        && matches!(
            task.state,
            TaskState::Runnable | TaskState::Blocked | TaskState::Unknown(_)
        )
}

fn state(task: &TaskSnapshot) -> String {
    state_text(task.detail.as_deref(), &task.state)
}

/// The runtime's words for what a task waits for, or else its state.
fn state_text(detail: Option<&str>, state: &TaskState) -> String {
    detail.map_or_else(
        || {
            match state {
                TaskState::Running => "running",
                TaskState::Runnable => "runnable",
                TaskState::Blocked => "blocked",
                TaskState::Exited => "exited",
                TaskState::Unknown(_) => "unknown",
            }
            .to_owned()
        },
        str::to_owned,
    )
}

fn labels(task: &TaskSnapshot) -> Option<String> {
    label_text(&task.labels)
}

/// A task's labels as `key=value` lines, if it has any.
fn label_text(labels: &[(Arc<str>, Arc<str>)]) -> Option<String> {
    (!labels.is_empty()).then(|| {
        labels
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

/// A backtrace's frames as a sample's stack would have them, with the
/// names of the futures it awaits: an inline frame is its physical frame's
/// lines, and tail calls, which only debug information shows, are left out.
fn frames(backtrace: &crate::Backtrace, futures: Vec<String>) -> Vec<WaitFrame> {
    let mut futures = futures.into_iter();
    let mut frames = Vec::with_capacity(backtrace.frames.len() + 1);
    // The innermost frame, and one a signal interrupted, are where their
    // code is; a caller's address returns past its call.
    let mut exact = true;
    for frame in backtrace.frames.iter() {
        let role = role_number(frame.role);
        match frame.kind {
            FrameKind::Inline | FrameKind::TailCall => {}
            FrameKind::Physical => {
                if let Some(instruction) = frame.instruction {
                    let address = instruction.get();
                    let address = if exact {
                        address
                    } else {
                        address.wrapping_sub(1)
                    };
                    frames.push(WaitFrame::Code { address, role });
                }
                exact = false;
            }
            FrameKind::Signal => {
                if let Some(instruction) = frame.instruction {
                    frames.push(WaitFrame::Code {
                        address: instruction.get(),
                        role,
                    });
                }
                exact = true;
            }
            FrameKind::Async { .. } => {
                if let Some(instruction) = frame.instruction {
                    frames.push(WaitFrame::Code {
                        address: instruction.get(),
                        role,
                    });
                }
            }
            FrameKind::Awaited { .. } => {
                let name = futures.next().unwrap_or_else(|| "awaiting a future".into());
                frames.push(WaitFrame::Awaited(name));
            }
        }
    }
    if let Some(reason) = truncation(&backtrace.termination) {
        frames.push(WaitFrame::Truncated(reason));
    }
    frames
}

/// Why a task's stack ends early, if it does.
const fn truncation(termination: &UnwindTermination) -> Option<Truncation> {
    Some(match termination {
        UnwindTermination::Complete => return None,
        UnwindTermination::NoUnwindInfo { .. } => Truncation::NoUnwindInfo,
        UnwindTermination::ModuleNotFound { .. } => Truncation::UnmappedAddress,
        UnwindTermination::InvalidCaller { .. } | UnwindTermination::CycleDetected => {
            Truncation::InvalidCaller
        }
        UnwindTermination::UnresolvedStackSwitch { .. } => Truncation::StackSwitch,
        UnwindTermination::BrokenAwaitChain { .. } => Truncation::AwaitChainEnded,
        UnwindTermination::DepthLimit => Truncation::DepthLimit,
        UnwindTermination::UnsupportedUnwindInfo { .. }
        | UnwindTermination::CorruptUnwindInfo { .. }
        | UnwindTermination::RegisterUnavailable { .. }
        | UnwindTermination::MemoryReadFailed { .. } => Truncation::Unreadable,
    })
}

/// The most bytes of a parked task's stack a live snapshot reads.
const LIVE_STACK_BYTES: u64 = 1 << 20;
/// The deepest stack a live snapshot unwinds.
const LIVE_FRAMES: usize = 512;

/// What a live snapshot found.
#[derive(Debug)]
pub enum Live {
    Taken(Snapshot),
    /// A runtime needs a thread's thread pointer that no stopped snapshot
    /// has learned.
    Unlearned,
    /// The process exited, before or while it was read, so it has no
    /// tasks, and what was read of it may show it partly gone.
    Exited,
}

impl Snapshotter {
    /// Reads where the process's tasks wait while it runs (see the module
    /// documentation).
    pub fn take_live(&self) -> Live {
        let _span = crate::span!("live task snapshot");
        let mut snapshot = Snapshot::default();
        let info = &self.program.debug_info;
        let image = Arc::clone(&info.image);
        let Some(bias) = super::perf::program_bias(self.pid) else {
            if exited(self.pid) {
                return Live::Exited;
            }
            snapshot
                .problems
                .push("the program's mapping cannot be read".to_owned());
            return Live::Taken(snapshot);
        };
        let threads = threads(self.pid);
        let pointers = threads
            .iter()
            .filter_map(|thread| {
                let tid = u32::try_from(thread.get()).ok()?;
                let start = thread_start(self.pid, tid)?;
                Some((tid, *self.thread_pointers.get(&(tid, start))?))
            })
            .collect();
        let stop = LiveStop {
            pid: self.pid,
            bias,
            threads,
            pointers,
            unlearned: AtomicBool::new(false),
        };
        let modules = [crate::unwind::UnwindModule {
            loaded: crate::LoadedModule {
                id: ModuleId::new(0),
                image: image.id(),
                load_bias: bias,
            },
            image: &image,
            unwind: info.unwind.as_ref(),
        }];
        let runtime_image: Arc<dyn runtime_model::RuntimeImage + Send + Sync> =
            Arc::clone(&image) as _;
        for model in runtime_model::detect(&runtime_image) {
            match model {
                Ok(model) => {
                    let live = LiveRuntime {
                        image: &image,
                        stop: &stop,
                        modules: &modules,
                        model: model.as_ref(),
                    };
                    live.read(&mut snapshot);
                }
                Err(unbound) => snapshot.problems.push(unbound.reason.to_string()),
            }
        }
        if exited(self.pid) {
            Live::Exited
        } else if stop.unlearned.load(Ordering::Relaxed) {
            Live::Unlearned
        } else {
            Live::Taken(snapshot)
        }
    }
}

/// Whether process `pid` has exited: it is gone, or waits to be reaped.
pub fn exited(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
        stat.rfind(')')
            .and_then(|end| stat.get(end + 2..))
            .is_some_and(|state| state.starts_with(['Z', 'X']))
    })
}

/// The threads of process `pid`, in the order of their ids.
fn threads(pid: u32) -> Vec<crate::ThreadId> {
    let mut threads: Vec<crate::ThreadId> = std::fs::read_dir(format!("/proc/{pid}/task"))
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .map(crate::ThreadId::new)
        .collect();
    threads.sort_unstable();
    threads
}

/// The running process, as a runtime model reads it: its memory, and of
/// its threads' registers, which run on, only the thread pointers stopped
/// snapshots learned.
struct LiveStop {
    pid: u32,
    bias: u64,
    threads: Vec<crate::ThreadId>,
    pointers: BTreeMap<u32, u64>,
    /// Whether a model asked for a thread pointer not learned.
    unlearned: AtomicBool,
}

impl RuntimeStop for LiveStop {
    fn read(&self, address: crate::VirtualAddress, bytes: &mut [u8]) -> bool {
        super::perf::read_memory(self.pid, address.get(), bytes)
            .is_ok_and(|read| read == bytes.len())
    }

    fn thread_pointer(&self, thread: crate::ThreadId) -> Option<u64> {
        let pointer = u32::try_from(thread.get())
            .ok()
            .and_then(|tid| self.pointers.get(&tid).copied());
        if pointer.is_none() {
            self.unlearned.store(true, Ordering::Relaxed);
        }
        pointer
    }

    fn instruction(&self, _thread: crate::ThreadId) -> Option<crate::VirtualAddress> {
        None
    }

    fn load_bias(&self) -> u64 {
        self.bias
    }

    fn threads(&self) -> Vec<crate::ThreadId> {
        self.threads.clone()
    }
}

/// A parked task's stack, read a page at a time as unwinding reaches it,
/// so that the pages read can be read again and compared.
struct LivePages<'a> {
    stop: &'a LiveStop,
    /// Where reading may begin, and the most bytes past it.
    start: u64,
    pages: BTreeMap<u64, Option<Box<[u8; PAGE]>>>,
}

const PAGE: usize = 4096;

impl LivePages<'_> {
    fn page(&mut self, page: u64) -> Option<&[u8; PAGE]> {
        let stop = self.stop;
        self.pages
            .entry(page)
            .or_insert_with(|| {
                let mut bytes = Box::new([0; PAGE]);
                stop.read(crate::VirtualAddress::new(page), &mut bytes[..])
                    .then_some(bytes)
            })
            .as_deref()
    }

    /// Whether every page read reads the same again.
    fn unchanged(&self) -> bool {
        self.pages.iter().all(|(&page, bytes)| {
            let mut again = [0; PAGE];
            let read = self.stop.read(crate::VirtualAddress::new(page), &mut again);
            bytes
                .as_ref()
                .map_or(!read, |bytes| read && **bytes == again)
        })
    }
}

impl crate::unwind::MemoryReader for LivePages<'_> {
    fn read_u64(&mut self, address: crate::VirtualAddress) -> Option<u64> {
        let address = address.get();
        let end = address.checked_add(8)?;
        if address < self.start || end > self.start.saturating_add(LIVE_STACK_BYTES) {
            return None;
        }
        let mut word = [0; 8];
        for (index, byte) in word.iter_mut().enumerate() {
            let at = address + index as u64;
            let page = at & !(PAGE as u64 - 1);
            *byte = self.page(page)?[usize::try_from(at - page).ok()?];
        }
        Some(u64::from_le_bytes(word))
    }
}

/// One runtime of the running process, as a live snapshot reads it.
struct LiveRuntime<'a> {
    image: &'a Arc<ModuleImage>,
    stop: &'a LiveStop,
    modules: &'a [crate::unwind::UnwindModule<'a>],
    model: &'a dyn runtime_model::RuntimeModel,
}

impl LiveRuntime<'_> {
    /// Adds the runtime's tasks, and where those that wait wait, to
    /// `snapshot`.
    fn read(&self, snapshot: &mut Snapshot) {
        let noun = self.model.task_noun();
        let mut start = 0;
        loop {
            let page = self.model.tasks(self.stop, start, 256, false);
            snapshot
                .problems
                .extend(page.gaps.iter().map(ToString::to_string));
            for task in &page.value.tasks {
                let labels = label_text(&task.labels);
                if task.locator != 0 {
                    snapshot.located.push(Located {
                        object: task.locator,
                        noun,
                        number: task.number,
                        labels: labels.clone(),
                    });
                }
                if !live_waits(task) {
                    continue;
                }
                match self.frames(task) {
                    Ok(Some(frames)) => snapshot.waiting.push(Waiting {
                        noun,
                        number: task.number,
                        state: state_text(task.detail.as_deref(), &task.state),
                        labels,
                        frames,
                    }),
                    Ok(None) => snapshot.changed += 1,
                    Err(error) => snapshot
                        .problems
                        .push(format!("{noun} {}: {error}", task.number)),
                }
            }
            match page.value.next {
                Some(next) if snapshot.located.len() < MOST_TASKS => start = next,
                Some(_) => {
                    snapshot
                        .problems
                        .push(format!("only the first {MOST_TASKS} tasks were read"));
                    return;
                }
                None => return,
            }
        }
    }

    /// Where a waiting task waits, or `None` when it changed while read.
    fn frames(&self, task: &runtime_model::RuntimeTask) -> Result<Option<Vec<WaitFrame>>, String> {
        let task_ref = runtime_model::TaskRef {
            number: task.number,
            locator: (task.locator != 0).then_some(task.locator),
        };
        let context = || self.model.task_context(self.stop, task_ref);
        let first = context().map_err(|error| error.to_string())?;
        let frames = match &first {
            None | Some(runtime_model::TaskContext::OnThread(_)) => return Ok(None),
            Some(runtime_model::TaskContext::Saved {
                registers,
                after_call,
            }) => {
                let Some((frames, pages)) = self.parked(registers, *after_call) else {
                    return Ok(None);
                };
                if !pages.unchanged() {
                    return Ok(None);
                }
                frames
            }
            Some(runtime_model::TaskContext::Suspended { future, ty }) => {
                let walk =
                    || runtime_model::futures::walk(self.image.as_ref(), self.stop, *future, *ty);
                let chain = walk();
                if walk() != chain {
                    return Ok(None);
                }
                self.awaits(&chain)
            }
        };
        let again = context().map_err(|error| error.to_string())?;
        Ok(same_context(first.as_ref(), again.as_ref()).then_some(frames))
    }

    /// A parked task's frames, unwound from the registers its runtime
    /// saved, with the pages of stack read for them.
    fn parked(
        &self,
        registers: &crate::unwind::RegisterFile,
        after_call: bool,
    ) -> Option<(Vec<WaitFrame>, LivePages<'_>)> {
        let mut pages = LivePages {
            stop: self.stop,
            start: registers.get(crate::unwind::RSP)?,
            pages: BTreeMap::new(),
        };
        let registers: Vec<(u16, u64)> = registers.iter().collect();
        let (unwound, end) =
            crate::unwind::sample::unwind_memory(self.modules, &registers, &mut pages, LIVE_FRAMES);
        let mut frames: Vec<WaitFrame> = unwound
            .iter()
            .enumerate()
            .map(|(index, frame)| WaitFrame::Code {
                // A task parked by a call is in that call, as a caller is.
                address: frame
                    .address
                    .get()
                    .wrapping_sub(u64::from(index == 0 && after_call)),
                role: role_number(frame.role),
            })
            .collect();
        if let crate::unwind::sample::StackEnd::Truncated(reason, _) = end {
            frames.push(WaitFrame::Truncated(reason));
        }
        Some((frames, pages))
    }

    /// A suspended task's chain of awaits as frames: each coroutine where
    /// it resumes, and the future it awaits by its type.
    fn awaits(&self, chain: &runtime_model::futures::AwaitChain) -> Vec<WaitFrame> {
        use runtime_model::futures::{AsyncFrameKind, ChainEnd};
        let bias = self.stop.bias;
        let mut frames = Vec::with_capacity(chain.frames.len() + 1);
        for frame in &chain.frames {
            match &frame.kind {
                AsyncFrameKind::Coroutine { state, .. } => {
                    let functions = self.image.coroutine_functions(frame.ty.id);
                    let role = functions
                        .first()
                        .map_or(crate::CodeRole::Ordinary, |function| function.info().role);
                    if let Some(address) =
                        crate::backend::resume_address(self.image, &functions, *state)
                    {
                        frames.push(WaitFrame::Code {
                            address: address.get().wrapping_add(bias),
                            role: role_number(role),
                        });
                    }
                }
                AsyncFrameKind::Leaf => {
                    let name = self
                        .image
                        .type_info(frame.ty)
                        .map_or_else(|| "a future".to_owned(), TypeInfo::qualified_name);
                    frames.push(WaitFrame::Awaited(format!("awaiting {name}")));
                }
            }
        }
        let reason = match chain.end {
            ChainEnd::Leaf | ChainEnd::Unresumed | ChainEnd::Finished => None,
            ChainEnd::NoAwaitee | ChainEnd::Broken(_) => Some(Truncation::AwaitChainEnded),
            ChainEnd::TooDeep => Some(Truncation::DepthLimit),
            ChainEnd::Cycle => Some(Truncation::InvalidCaller),
        };
        frames.extend(reason.map(WaitFrame::Truncated));
        frames
    }
}

/// Whether two reads of a task's context found it where it was.
fn same_context(
    first: Option<&runtime_model::TaskContext>,
    again: Option<&runtime_model::TaskContext>,
) -> bool {
    use runtime_model::TaskContext;
    match (first, again) {
        (
            Some(TaskContext::Saved {
                registers,
                after_call,
            }),
            Some(TaskContext::Saved {
                registers: other,
                after_call: other_call,
            }),
        ) => registers == other && after_call == other_call,
        (
            Some(TaskContext::Suspended { future, ty }),
            Some(TaskContext::Suspended {
                future: other,
                ty: other_ty,
            }),
        ) => future == other && ty == other_ty,
        _ => false,
    }
}

/// Whether a task a live snapshot lists waits: as [`waits`] says, and on
/// no thread, since a running thread's registers are not read.
const fn live_waits(task: &runtime_model::RuntimeTask) -> bool {
    !task.internal
        && task.thread.is_none()
        && matches!(
            task.state,
            TaskState::Runnable | TaskState::Blocked | TaskState::Unknown(_)
        )
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{Live, Snapshotter};

    /// Whether process `pid` has exited and waits to be reaped.
    fn zombie(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| Some(stat[stat.rfind(')')? + 2..].starts_with('Z')))
            .unwrap_or(false)
    }

    /// A process that has exited, before it is reaped, keeps no mappings
    /// and runs no tasks: a live snapshot of it says it exited, rather
    /// than that it could not read the program.
    #[test]
    fn a_live_snapshot_of_an_exited_process_says_it_exited() {
        let program =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("build/test-programs/basic");
        let mut child = std::process::Command::new(&program)
            .spawn()
            .expect("start the program");
        let pid = child.id();
        let snapshotter = Snapshotter::new(pid, &program, &crate::DebugFileOptions::default())
            .expect("read the program");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !zombie(pid) {
            assert!(Instant::now() < deadline, "the program never exited");
            std::thread::yield_now();
        }
        let live = snapshotter.take_live();
        child.wait().expect("reap the program");
        assert!(matches!(live, Live::Exited), "{live:?}");
    }
}
