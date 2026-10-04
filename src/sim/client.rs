//! The simulated user: an async task that drives the real
//! [`DebuggerHandle`], chooses what to do from the `Client` stream, and
//! checks every answer against what the protocol promises.
//!
//! The client is the only actor besides the program, so it knows what
//! state the debugger is in when it asks; an error the request cannot
//! legitimately produce in that state fails the run. The world polls the
//! task by hand, so it never runs except as a scheduled action.
#![expect(
    clippy::future_not_send,
    reason = "the world polls the client by hand on its one thread"
)]

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use tokio::sync::{broadcast, oneshot};

use super::choices::{Choices, Stream};
use super::kernel::Tid;
use super::loader::Image;
use super::marks::{Mark, Marks};
use super::report::Failure;
use crate::backend::ControllerMessage;
use crate::protocol::Request;
use crate::{
    BreakpointId, BreakpointLocation, BreakpointSpec, DebuggerEvent, DebuggerHandle, Error,
    ExceptionDisposition, ExecutionId, InferiorState, LaunchOptions, LineNumber,
    MemoryReadCompletion, ModuleId, PresentedFrame, ProcessId, ResumeScope, StateSnapshot,
    StepKind, StopId, StopReason, ThreadState, UnwindTermination, VirtualAddress,
};

/// The most bytes one memory read asks for.
const MAX_READ: u64 = 64;

/// What the client knows about the program it debugs.
pub struct Script {
    pub arguments: Vec<String>,
    pub stop_at_entry: bool,
    pub requests: u64,
    pub launches: u64,
    pub early_breakpoints: u64,
    /// Functions the program defines in some variant.
    pub functions: Vec<String>,
    /// Functions this variant defines, which must resolve.
    pub defined: Vec<String>,
    pub source: PathBuf,
    pub source_lines: u64,
    /// The image, whose code memory reads must show unchanged.
    pub image: Arc<Image>,
}

/// What the client shares with the world.
#[derive(Clone, Default)]
pub struct Shared {
    /// What the client did, for the trace.
    pub notes: Rc<RefCell<Vec<String>>>,
    /// Processes something outside the session killed, which the world
    /// records when its fault fires.
    pub killed: Rc<RefCell<BTreeSet<Tid>>>,
    /// Processes that began to end as a whole, killed or exiting their
    /// group, which the world records as it happens.
    pub ending: Rc<RefCell<BTreeSet<Tid>>>,
    /// The user's breakpoints the debugger said it made, and the client has
    /// not asked to remove, with the addresses of their locations.
    pub breakpoints: Rc<RefCell<BTreeMap<u64, BTreeSet<u64>>>>,
}

impl Shared {
    /// Every address where a user breakpoint is certainly enabled.
    #[must_use]
    pub fn addresses(&self) -> BTreeSet<u64> {
        self.breakpoints
            .borrow()
            .values()
            .flatten()
            .copied()
            .collect()
    }
}

pub struct Client {
    pub handle: DebuggerHandle,
    pub choices: Rc<RefCell<Choices>>,
    pub marks: Rc<RefCell<Marks>>,
    pub shared: Shared,
    pub script: Script,
    /// The last execution that resumed one thread alone.
    pub alone: Cell<Option<ExecutionId>>,
}

/// A breakpoint the client added, with the image addresses of its traps.
struct Added {
    id: BreakpointId,
    traps: Vec<u64>,
}

fn protocol(message: impl Into<String>) -> Failure {
    Failure::debugger("protocol", message)
}

impl Client {
    fn draw(&self, bound: u64) -> u64 {
        self.choices.borrow_mut().below(Stream::Client, bound)
    }

    fn note(&self, note: impl Into<String>) {
        self.shared.notes.borrow_mut().push(note.into());
    }

    /// Whether something outside the session killed `process`.
    fn killed(&self, process: ProcessId) -> bool {
        Tid::try_from(process.get()).is_ok_and(|tgid| self.shared.killed.borrow().contains(&tgid))
    }

    /// Accepts a failure of a request about `process` if the process began
    /// to end as a whole: killed from outside, or exiting its group. Such a
    /// process vanishes under requests about it, even from a stop the
    /// debugger published just before it heard of the end, so a request
    /// may fail however it fails.
    fn excuse(&self, process: ProcessId, result: Result<(), Failure>) -> Result<(), Failure> {
        let ending = Tid::try_from(process.get())
            .is_ok_and(|tgid| self.shared.ending.borrow().contains(&tgid));
        match result {
            Err(failure) if failure.check == "protocol" && ending => {
                self.note(format!(
                    "the program is ending, so this may fail: {}",
                    failure.message
                ));
                Ok(())
            }
            result => result,
        }
    }

    fn mark(&self, mark: Mark) {
        self.marks.borrow_mut().hit(mark);
    }

    /// Runs the session to its end: requests, then a shutdown.
    pub async fn run(self) -> Result<(), Failure> {
        let mut events = self.handle.subscribe();
        let mut breakpoints = Vec::new();
        for _ in 0..self.script.early_breakpoints {
            self.add_breakpoint(&mut breakpoints).await?;
        }
        let mut launches = 0;
        let mut last_stop: Option<StopId> = None;
        let mut stale: Option<StopId> = None;
        for _ in 0..self.script.requests {
            let snapshot = self
                .handle
                .snapshot()
                .await
                .map_err(|error| protocol(format!("snapshot failed: {error}")))?;
            match snapshot.inferior.clone() {
                InferiorState::NotRunning => {
                    if launches == self.script.launches {
                        break;
                    }
                    launches += 1;
                    if launches > 1 {
                        self.mark(Mark::Relaunched);
                    }
                    self.launch().await?;
                }
                InferiorState::Running {
                    process_id,
                    execution_id,
                } => {
                    let result = self
                        .while_running(execution_id, &mut events, &mut breakpoints)
                        .await;
                    self.excuse(process_id, result)?;
                }
                InferiorState::Stopped {
                    process_id,
                    stop_id,
                    reason,
                    ..
                } => {
                    if last_stop != Some(stop_id) {
                        match reason {
                            StopReason::Entry => self.mark(Mark::EntryStop),
                            StopReason::Breakpoint { .. } => self.mark(Mark::BreakpointStop),
                            StopReason::Step { .. } => self.mark(Mark::StepStop),
                            StopReason::Pause => self.mark(Mark::PauseStop),
                            // No golden program raises a signal or does
                            // anything the debugger cannot classify.
                            StopReason::Exception(_) | StopReason::Unclassifiable { .. }
                                if !self.killed(process_id) =>
                            {
                                return Err(protocol(format!("stop {stop_id} reports {reason:?}")));
                            }
                            _ => {}
                        }
                        let at_breakpoints = snapshot
                            .threads
                            .iter()
                            .filter(|thread| {
                                matches!(
                                    thread.state,
                                    ThreadState::Stopped {
                                        reason: Some(StopReason::Breakpoint { .. })
                                    }
                                )
                            })
                            .count();
                        if at_breakpoints > 1 {
                            self.mark(Mark::CoHit);
                        }
                    }
                    if let Some(previous) = last_stop
                        && previous != stop_id
                    {
                        if stop_id.get() < previous.get() {
                            return Err(protocol(format!(
                                "stop {stop_id} follows stop {previous}"
                            )));
                        }
                        stale = Some(previous);
                    }
                    last_stop = Some(stop_id);
                    let result = self
                        .while_stopped(&snapshot, stop_id, stale, &mut events, &mut breakpoints)
                        .await;
                    self.excuse(process_id, result)?;
                }
            }
        }
        self.shutdown().await
    }

    async fn launch(&self) -> Result<(), Failure> {
        let options = LaunchOptions {
            arguments: self.script.arguments.iter().map(Into::into).collect(),
            stop_at_entry: self.script.stop_at_entry,
            ..LaunchOptions::default()
        };
        let killed = self.shared.killed.borrow().len();
        match self.handle.launch_with(options).await {
            Ok(execution) => self.note(format!("launched as execution {execution}")),
            // Killed from outside before its first stop, the program never
            // finishes launching.
            Err(error) if self.shared.killed.borrow().len() > killed => {
                self.note(format!("the program was killed from outside: {error}"));
            }
            Err(error) => return Err(protocol(format!("launch failed: {error}"))),
        }
        Ok(())
    }

    async fn while_running(
        &self,
        execution: Option<ExecutionId>,
        events: &mut broadcast::Receiver<DebuggerEvent>,
        breakpoints: &mut Vec<Added>,
    ) -> Result<(), Failure> {
        match self.draw(6) {
            0 => {
                self.note("pause");
                match self.handle.pause().await {
                    Ok(reason) => self.note(format!("paused: {reason:?}")),
                    Err(Error::EventStreamLagged(_)) => self.mark(Mark::ClientLagged),
                    // The program may stop or end before the pause arrives.
                    Err(Error::AlreadyStopped | Error::NotRunning) => {}
                    Err(error) => return Err(protocol(format!("pause failed: {error}"))),
                }
            }
            1 => {
                if self.add_breakpoint(breakpoints).await? {
                    self.mark(Mark::EditWhileRunning);
                }
            }
            2 => {
                if self.remove_breakpoint(breakpoints).await? {
                    self.mark(Mark::EditWhileRunning);
                }
            }
            3 => {
                self.mark(Mark::KilledRunning);
                self.kill(true).await?;
            }
            _ => {
                // A thread running alone may wait forever for a sibling
                // that stays stopped, as the program's own lock would make
                // it, so only an execution of every thread is waited for.
                if let Some(execution) = execution
                    && self.alone.get() != Some(execution)
                {
                    self.note(format!("wait for execution {execution}"));
                    self.wait_for(execution, events).await?;
                }
            }
        }
        Ok(())
    }

    async fn while_stopped(
        &self,
        snapshot: &StateSnapshot,
        stop: StopId,
        stale: Option<StopId>,
        events: &mut broadcast::Receiver<DebuggerEvent>,
        breakpoints: &mut Vec<Added>,
    ) -> Result<(), Failure> {
        let InferiorState::Stopped { process_id, .. } = snapshot.inferior else {
            unreachable!("the client acts while stopped on a stopped snapshot")
        };
        let scope = ResumeScope::Process(process_id);
        match self.draw(12) {
            0 => {
                self.note("resume");
                match self.handle.resume().await {
                    Ok(reason) => self.note(format!("resumed until {reason:?}")),
                    Err(Error::EventStreamLagged(_)) => self.mark(Mark::ClientLagged),
                    Err(error) => return Err(protocol(format!("resume failed: {error}"))),
                }
            }
            1 => {
                let execution = self
                    .handle
                    .continue_execution(stop, scope, ExceptionDisposition::Pass)
                    .await
                    .map_err(|error| protocol(format!("continue from {stop} failed: {error}")))?;
                self.note(format!("continued from {stop} as execution {execution}"));
                self.wait_for(execution, events).await?;
            }
            2 | 3 => self.step().await?,
            4 => {
                self.add_breakpoint(breakpoints).await?;
            }
            5 => {
                self.remove_breakpoint(breakpoints).await?;
            }
            6 => self.read_code(breakpoints).await?,
            7 => self.backtrace().await?,
            8 => {
                self.mark(Mark::KilledStopped);
                self.kill(false).await?;
            }
            9 => self.select_thread(snapshot).await?,
            10 => self.continue_thread(snapshot, stop).await?,
            _ => {
                if let Some(stale) = stale {
                    self.continue_stale(stale, scope).await?;
                }
            }
        }
        Ok(())
    }

    /// Selects one of the stopped threads, which later steps, backtraces,
    /// and reads use.
    async fn select_thread(&self, snapshot: &StateSnapshot) -> Result<(), Failure> {
        let thread = self
            .choices
            .borrow_mut()
            .pick(Stream::Client, &snapshot.threads)
            .id;
        self.handle
            .select_thread(thread)
            .await
            .map_err(|error| protocol(format!("selecting thread {thread} failed: {error}")))?;
        self.note(format!("selected thread {thread}"));
        if snapshot.selected_thread != Some(thread) {
            self.mark(Mark::ThreadSelected);
        }
        Ok(())
    }

    /// Resumes one stopped thread alone. Its execution may never end by
    /// itself, so the client does not wait for it.
    async fn continue_thread(&self, snapshot: &StateSnapshot, stop: StopId) -> Result<(), Failure> {
        let thread = self
            .choices
            .borrow_mut()
            .pick(Stream::Client, &snapshot.threads)
            .id;
        let execution = self
            .handle
            .continue_execution(
                stop,
                ResumeScope::Thread(thread),
                ExceptionDisposition::Pass,
            )
            .await
            .map_err(|error| protocol(format!("continuing thread {thread} failed: {error}")))?;
        self.note(format!(
            "continued thread {thread} alone as execution {execution}"
        ));
        self.mark(Mark::ThreadContinued);
        self.alone.set(Some(execution));
        Ok(())
    }

    /// Takes a backtrace, which a stop whose inline frame is ambiguous
    /// cannot present.
    async fn backtrace(&self) -> Result<(), Failure> {
        let snapshot = self
            .handle
            .snapshot()
            .await
            .map_err(|error| protocol(format!("snapshot failed: {error}")))?;
        match self.handle.backtrace().await {
            Ok(backtrace) => self.note(format!(
                "backtrace: {} frames, {:?}",
                backtrace.frames.len(),
                backtrace.termination
            )),
            Err(Error::AmbiguousInlineFrame) if presented_ambiguously(&snapshot) => {
                self.note("backtrace from an ambiguous inline frame refused");
            }
            Err(error) => return Err(protocol(format!("backtrace failed: {error}"))),
        }
        Ok(())
    }

    async fn step(&self) -> Result<(), Failure> {
        let kinds = [
            StepKind::Instruction,
            StepKind::OverInstruction,
            StepKind::IntoSource,
            StepKind::OverSource,
            StepKind::Out,
        ];
        let kind = *self.choices.borrow_mut().pick(Stream::Client, &kinds);
        let before = self
            .handle
            .snapshot()
            .await
            .map_err(|error| protocol(format!("snapshot failed: {error}")))?;
        // A source step must know which frame it starts in, so one from a
        // stop whose inline frame is ambiguous is refused, not guessed.
        let ambiguous = kind != StepKind::Instruction
            && kind != StepKind::OverInstruction
            && presented_ambiguously(&before);
        // Stepping out of a frame the unwind information says has no caller
        // must be refused, and change nothing.
        let outermost = if kind == StepKind::Out && !ambiguous {
            let backtrace = self
                .handle
                .backtrace()
                .await
                .map_err(|error| protocol(format!("backtrace failed: {error}")))?;
            (backtrace.frames.len() == 1).then_some(backtrace.termination)
        } else {
            None
        };
        self.note(format!("step {kind:?}"));
        let result = self.handle.step(kind).await;
        if ambiguous {
            if !matches!(result, Err(Error::AmbiguousInlineFrame)) {
                return Err(protocol(format!(
                    "step {kind:?} from an ambiguous inline frame returned {result:?}"
                )));
            }
            let after = self
                .handle
                .snapshot()
                .await
                .map_err(|error| protocol(format!("snapshot failed: {error}")))?;
            if after != before {
                return Err(protocol(format!(
                    "a refused step changed the state from {before:?} to {after:?}"
                )));
            }
            self.note(format!(
                "step {kind:?} from an ambiguous inline frame refused"
            ));
            return Ok(());
        }
        match (result, outermost) {
            (Ok(reason), None | Some(UnwindTermination::NoUnwindInfo { .. })) => {
                self.note(format!("stepped: {reason:?}"));
            }
            (Err(Error::EventStreamLagged(_)), _) => self.mark(Mark::ClientLagged),
            (Err(error), Some(termination)) => {
                let after = self
                    .handle
                    .snapshot()
                    .await
                    .map_err(|error| protocol(format!("snapshot failed: {error}")))?;
                if after != before {
                    return Err(protocol(format!(
                        "a refused step out changed the state from {before:?} to {after:?}"
                    )));
                }
                self.note(format!(
                    "step out of the outermost frame ({termination}) refused: {error}"
                ));
                self.mark(Mark::StepOutRefused);
            }
            (Ok(reason), Some(termination)) => {
                return Err(protocol(format!(
                    "stepped out of a frame without a caller ({termination}): {reason:?}"
                )));
            }
            (Err(error), None) => {
                return Err(protocol(format!("step {kind:?} failed: {error}")));
            }
        }
        Ok(())
    }

    /// Adds a breakpoint, returning whether the debugger made one.
    async fn add_breakpoint(&self, breakpoints: &mut Vec<Added>) -> Result<bool, Failure> {
        let spec = if self.draw(2) == 0 {
            let function = self
                .choices
                .borrow_mut()
                .pick(Stream::Client, &self.script.functions)
                .clone();
            BreakpointSpec::Function(function)
        } else {
            let line = self.draw(self.script.source_lines) + 1;
            BreakpointSpec::Source {
                path: self.script.source.clone(),
                line: LineNumber::new(line).expect("lines count from one"),
            }
        };
        let breakpoint = match self.handle.add_breakpoint(spec.clone()).await {
            Ok(breakpoint) => breakpoint,
            // A line outside every function names no code.
            Err(Error::SourceLineUnavailable { line, .. }) if matches!(&spec, BreakpointSpec::Source { line: wanted, .. } if wanted.get() == line) =>
            {
                self.note(format!("{spec} has no code"));
                return Ok(false);
            }
            // Another variant may define a function this one inlined away.
            Err(Error::FunctionNotFound(name) | Error::SymbolNotFound(name))
                if matches!(&spec, BreakpointSpec::Function(wanted) if *wanted == name)
                    && !self.script.defined.contains(&name) =>
            {
                self.note(format!("{name} is not in this variant"));
                return Ok(false);
            }
            Err(error) => {
                return Err(protocol(format!(
                    "adding a breakpoint at {spec} failed: {error}"
                )));
            }
        };
        self.note(format!(
            "breakpoint {} at {spec}: {} locations",
            breakpoint.id,
            breakpoint.locations.len()
        ));
        let traps = breakpoint
            .locations
            .iter()
            .filter_map(|location| match location.location {
                BreakpointLocation::Image(address) => Some(address.get()),
                BreakpointLocation::Virtual(_) => None,
            })
            .collect::<Vec<_>>();
        // Golden programs are static executables, which load where their
        // images say.
        self.shared
            .breakpoints
            .borrow_mut()
            .insert(breakpoint.id.get(), traps.iter().copied().collect());
        breakpoints.retain(|added| added.id != breakpoint.id);
        breakpoints.push(Added {
            id: breakpoint.id,
            traps,
        });
        Ok(true)
    }

    /// Removes a breakpoint, returning whether there was one to remove.
    async fn remove_breakpoint(&self, breakpoints: &mut Vec<Added>) -> Result<bool, Failure> {
        if breakpoints.is_empty() {
            return Ok(false);
        }
        let index = usize::try_from(self.draw(breakpoints.len() as u64)).expect("small");
        let id = breakpoints.swap_remove(index).id;
        // From the moment the client asks, the breakpoint may be gone.
        self.shared.breakpoints.borrow_mut().remove(&id.get());
        self.handle
            .remove_breakpoint(id)
            .await
            .map_err(|error| protocol(format!("removing breakpoint {id} failed: {error}")))?;
        self.note(format!("removed breakpoint {id}"));
        Ok(true)
    }

    /// The load bias of the main executable.
    async fn main_bias(&self) -> Result<u64, Failure> {
        let modules = self
            .handle
            .loaded_modules()
            .await
            .map_err(|error| protocol(format!("listing modules failed: {error}")))?;
        modules
            .modules
            .iter()
            .find(|record| record.module.id == ModuleId::new(0))
            .map(|record| record.module.load_bias)
            .ok_or_else(|| protocol("no main module is loaded"))
    }

    /// Reads code, which must show the program's own bytes whatever traps
    /// the debugger planted. Half the reads aim at a breakpoint's trap.
    async fn read_code(&self, breakpoints: &[Added]) -> Result<(), Failure> {
        let pages = self.script.image.code().collect::<Vec<_>>();
        let traps = breakpoints
            .iter()
            .flat_map(|added| added.traps.iter().copied())
            .collect::<Vec<_>>();
        let length = self.draw(MAX_READ) + 1;
        let aim = if !traps.is_empty() && self.draw(2) == 0 {
            let image = *self.choices.borrow_mut().pick(Stream::Client, &traps);
            Some(image + self.main_bias().await?)
        } else {
            None
        };
        let (page_address, page) = match aim {
            Some(trap) => *pages
                .iter()
                .find(|(start, page)| (*start..*start + page.len() as u64).contains(&trap))
                .ok_or_else(|| protocol(format!("trap {trap:#x} is outside the code")))?,
            None => pages[usize::try_from(self.draw(pages.len() as u64)).expect("small")],
        };
        let offset = aim
            .map_or_else(
                || self.draw(page.len() as u64 - length + 1),
                |trap| (trap - page_address).saturating_sub(self.draw(length)),
            )
            .min(page.len() as u64 - length);
        let address = page_address + offset;
        let read = self
            .handle
            .read_memory(VirtualAddress::new(address), length)
            .await
            .map_err(|error| protocol(format!("reading {address:#x} failed: {error}")))?;
        let start = usize::try_from(offset).expect("small");
        let expected = &page[start..start + usize::try_from(length).expect("small")];
        if read.completion != MemoryReadCompletion::Complete || &*read.bytes != expected {
            return Err(Failure::debugger(
                "memory",
                format!(
                    "reading {length} bytes of code at {address:#x} returned {:02x?} ({:?}); \
                     the program has {expected:02x?}",
                    read.bytes, read.completion
                ),
            ));
        }
        self.note(format!("read {length} bytes at {address:#x}"));
        if aim.is_some_and(|trap| (address..address + length).contains(&trap)) {
            self.mark(Mark::ReadOverTrap);
        }
        Ok(())
    }

    /// Kills the program. One that was running may end on its own before
    /// the kill arrives.
    async fn kill(&self, running: bool) -> Result<(), Failure> {
        self.note("kill");
        match self.handle.kill().await {
            Ok(()) => {}
            Err(Error::NotRunning) if running => self.note("the program ended first"),
            Err(error) => return Err(protocol(format!("kill failed: {error}"))),
        }
        let snapshot = self
            .handle
            .snapshot()
            .await
            .map_err(|error| protocol(format!("snapshot failed: {error}")))?;
        if snapshot.inferior != InferiorState::NotRunning {
            return Err(protocol(format!(
                "after a kill the inferior is {:?}",
                snapshot.inferior
            )));
        }
        Ok(())
    }

    /// Continuing from a stop that is no longer current must be refused,
    /// and must change nothing.
    async fn continue_stale(&self, stale: StopId, scope: ResumeScope) -> Result<(), Failure> {
        let before = self
            .handle
            .snapshot()
            .await
            .map_err(|error| protocol(format!("snapshot failed: {error}")))?;
        match self
            .handle
            .continue_execution(stale, scope, ExceptionDisposition::Pass)
            .await
        {
            Err(Error::StaleStop) => {}
            other => {
                return Err(protocol(format!(
                    "continuing from stale stop {stale} returned {other:?}"
                )));
            }
        }
        let after = self
            .handle
            .snapshot()
            .await
            .map_err(|error| protocol(format!("snapshot failed: {error}")))?;
        if after != before {
            return Err(protocol(format!(
                "a refused request changed the state from {before:?} to {after:?}"
            )));
        }
        self.note(format!("stale stop {stale} refused"));
        self.mark(Mark::StaleStopRefused);
        Ok(())
    }

    /// Waits until `execution` stops or ends. A receiver that lagged may
    /// have missed it, so the state decides then.
    async fn wait_for(
        &self,
        execution: ExecutionId,
        events: &mut broadcast::Receiver<DebuggerEvent>,
    ) -> Result<(), Failure> {
        loop {
            match events.recv().await {
                Ok(
                    DebuggerEvent::InferiorStopped {
                        execution_id: Some(ended),
                        ..
                    }
                    | DebuggerEvent::InferiorExited {
                        execution_id: Some(ended),
                        ..
                    },
                ) if ended == execution => return Ok(()),
                Ok(DebuggerEvent::InferiorDetached { .. }) => return Ok(()),
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    self.mark(Mark::ClientLagged);
                    let snapshot = self
                        .handle
                        .snapshot()
                        .await
                        .map_err(|error| protocol(format!("snapshot failed: {error}")))?;
                    if !matches!(
                        snapshot.inferior,
                        InferiorState::Running { execution_id: Some(running), .. }
                            if running == execution
                    ) {
                        return Ok(());
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(protocol("the event channel closed"));
                }
            }
        }
    }

    async fn shutdown(&self) -> Result<(), Failure> {
        self.note("shutdown");
        let (reply, answer) = oneshot::channel();
        self.handle
            .requests
            .send(ControllerMessage::Request(Request::Shutdown { reply }))
            .await
            .map_err(|_| protocol("the request queue closed before shutdown"))?;
        answer
            .await
            .map_err(|_| protocol("shutdown was never answered"))?
            .map_err(|error| protocol(format!("shutdown failed: {error}")))
    }
}

/// Whether a stop presents its selected thread in an inline frame the
/// debug information leaves ambiguous.
fn presented_ambiguously(snapshot: &StateSnapshot) -> bool {
    snapshot
        .presentation
        .as_ref()
        .is_some_and(|presentation| matches!(presentation.frame, PresentedFrame::Ambiguous(_)))
}
