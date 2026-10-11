//! Task snapshots: where an async program's tasks wait.
//!
//! A task that waits runs no code, so no CPU sample sees it. A snapshot
//! stops the program through the debugger's attach, lists every task of
//! its runtimes, reads each waiting task's stack, and detaches, leaving
//! the program to go on as it was. Each wait then stands for the time
//! since the snapshot before.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use super::RecordError;
use crate::capture::records::{Truncation, role_number};
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

/// One snapshot's waiting tasks, and why it may miss some.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub waiting: Vec<Waiting>,
    /// Each task it could not read, or part of the runtime's tasks, and
    /// why.
    pub problems: Vec<String>,
}

/// Takes snapshots of one process.
pub struct Snapshotter {
    pid: u32,
    program: Program,
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
        Ok(Self { pid, program })
    }

    /// Stops the process, reads where its tasks wait, and lets it go on.
    /// A thread of its own runs the snapshot in a runtime of its own, as
    /// the caller may run in one.
    pub fn take(&self) -> Result<Snapshot, crate::Error> {
        let _span = crate::span!("task snapshot");
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?
                        .block_on(self.read())
                })
                .join()
                .unwrap_or_else(|_| Err(crate::Error::RequestCancelled))
        })
    }

    async fn read(&self) -> Result<Snapshot, crate::Error> {
        {
            let debugger = Debugger::for_program(self.program.clone())?;
            let handle = debugger.handle();
            let read = match handle
                .attach_process(ProcessId::new(u64::from(self.pid)))
                .await
            {
                Ok(stop) => read(&handle, stop).await,
                Err(error) => Err(error),
            };
            // The process goes on whether or not reading it failed.
            let detached = debugger.shutdown().await;
            let snapshot = read?;
            detached?;
            Ok(snapshot)
        }
    }
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
    task.detail.as_deref().map_or_else(
        || {
            match &task.state {
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
    (!task.labels.is_empty()).then(|| {
        task.labels
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
