//! The simulated user: an async task that drives the real
//! [`DebuggerHandle`], chooses what to do from the `Client` stream, and
//! checks every answer against what the protocol promises.
//!
//! The client is the only actor besides the program, so it knows what
//! state the debugger is in when it asks; an error the request cannot
//! legitimately produce in that state fails the run. The world polls the
//! task by hand, so it never runs except as a scheduled action.
//!
//! The session loop and the program's lifecycle live here; the user's
//! breakpoints and watchpoints in `breakpoints`, and what the client
//! inspects at a stop for the semantic oracles in `stops`.
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

use self::breakpoints::Added;
use super::choices::{Choices, Stream};
use super::hits::{Baseline, Published};
use super::kernel::DebugBehavior;
use super::kernel::Tid;
use super::loader::Image;
use super::markers::Marker;
use super::marks::{Mark, Marks};
use super::report::Failure;
use super::watches::Intent;
use crate::backend::ControllerMessage;
use crate::protocol::Request;
use crate::{
    Backtrace, DebuggerEvent, DebuggerHandle, Error, ExceptionDisposition, ExecutionContext,
    ExecutionId, FramePresentation, HeldChildren, InferiorState, LaunchOptions,
    MemoryReadCompletion, ModuleId, ProcessId, ResumeScope, StateSnapshot, StepKind, StopId,
    StopReason, ThreadId, ThreadState, VariableSnapshot, VirtualAddress,
};

pub use self::adopter::Adopter;

mod adopter;
mod breakpoints;
mod stops;

/// The most bytes one memory read asks for.
const MAX_READ: u64 = 64;

/// What the client knows about the program it debugs.
pub struct Script {
    pub arguments: Vec<String>,
    pub stop_at_entry: bool,
    pub requests: u64,
    pub launches: u64,
    pub early_breakpoints: u64,
    /// Whether to favor watching memory.
    pub watching: bool,
    /// Functions the program defines in some variant.
    pub functions: Vec<String>,
    /// Functions this variant defines, which must resolve.
    pub defined: Vec<String>,
    pub source: PathBuf,
    pub source_lines: u64,
    /// The source's markers, by line.
    pub markers: BTreeMap<u64, Marker>,
    /// In unoptimized code, the image addresses where a row of a marker's
    /// line starts, with the line: where its condition holds at every hit.
    pub marker_rows: BTreeMap<u64, u64>,
    /// The image, whose code memory reads must show unchanged.
    pub image: Arc<Image>,
    /// Small objects the program defines, which it may watch: name, image
    /// address, and size.
    pub globals: Vec<(String, u64, u64)>,
    /// The views the program's types are presented with, which the client
    /// loads into the session.
    pub views: Option<Arc<str>>,
    /// How the kernel answers debug-register requests.
    pub debug: DebugBehavior,
    /// The program the world started untraced, which the client attaches
    /// to rather than launching it first.
    pub attach: Option<ProcessId>,
    /// Whether the session holds the children the program forks, for
    /// sessions of their own.
    pub follow: bool,
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
    /// The user's breakpoints the debugger said it made enabled, and the
    /// client has not asked to remove or disable, with the image addresses
    /// of their locations. Temporary ones, which a stop may delete, are not
    /// among them.
    pub breakpoints: Rc<RefCell<BTreeMap<u64, BTreeSet<u64>>>>,
    /// The breakpoints the debugger said are disabled, which the client has
    /// not asked to enable or remove since.
    pub disabled: Rc<RefCell<BTreeSet<u64>>>,
    /// What the client saw that the world's oracles judge, in order.
    pub observations: Rc<RefCell<Vec<Observation>>>,
    /// The events about breakpoint hits the debugger published, which the
    /// world's auditor counts.
    pub published: Rc<RefCell<Published>>,
    /// The watchpoints the debugger said it armed, and the client has not
    /// asked to remove, by identifier.
    pub watches: Rc<RefCell<BTreeMap<u64, Intent>>>,
    /// Whether the session holds the children the program forks.
    pub following: Rc<Cell<bool>>,
    /// The children the session held, which the world hands to sessions of
    /// their own.
    pub held: Rc<RefCell<Option<HeldChildren>>>,
}

/// Something the client saw that an oracle judges against the simulation.
#[derive(Debug)]
pub enum Observation {
    /// A backtrace taken at a stop.
    Backtrace { stop: StopId, backtrace: Backtrace },
    /// A step of the selected thread, about to be requested, from where the
    /// debugger presented it.
    StepBegins {
        thread: ThreadId,
        kind: StepKind,
        presentation: Option<FramePresentation>,
        /// For an advance, the image addresses it runs to.
        targets: BTreeSet<u64>,
    },
    /// What the step ended with, when it ended without failing.
    StepEnded(Option<StopReason>),
    /// The variables of the selected frame at a stop, with the backtrace of
    /// their thread.
    Variables {
        stop: StopId,
        variables: VariableSnapshot,
        backtrace: Backtrace,
        /// Expressions evaluated in the same frame at the same stop.
        evaluations: Vec<Evaluated>,
    },
    /// A container a view presents, as a global's value, with its elements
    /// in one page and in pages of a smaller size.
    Presented {
        stop: StopId,
        name: String,
        value: Box<crate::InspectedValue>,
        whole: Result<Vec<crate::ValueChildPage>, String>,
        paged: Result<Vec<crate::ValueChildPage>, String>,
    },
}

/// An expression the client evaluated where it read variables.
#[derive(Debug)]
pub struct Evaluated {
    pub purpose: Purpose,
    pub text: String,
    pub result: Result<crate::Evaluation, String>,
}

/// What an evaluated expression asks, which decides how it is judged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Purpose {
    /// The condition of the marker on the frame's line, or its negation.
    Marker { negated: bool },
    /// What the marker on the frame's line expects.
    Expected,
    /// A variable shown once among the frame's variables: its name, or
    /// the name's address dereferenced.
    Name(String),
    /// The address of a variable shown once, in memory.
    Address(String),
    /// An integer variable cast to an integer type `bits` wide.
    Cast {
        name: String,
        bits: u32,
        signed: bool,
    },
    /// An expression no program's types allow.
    IllTyped,
    /// Two integer variables combined: `left operator right`.
    Arithmetic {
        left: String,
        operator: char,
        right: String,
    },
}

impl Shared {
    /// The user's breakpoints, with their locations where an image loaded
    /// `bias` above its own addresses puts them.
    #[must_use]
    pub fn intent(&self, bias: u64) -> BTreeMap<u64, BTreeSet<u64>> {
        self.breakpoints
            .borrow()
            .iter()
            .map(|(&id, addresses)| (id, addresses.iter().map(|address| address + bias).collect()))
            .collect()
    }

    /// Every address where a user breakpoint is certainly enabled, in an
    /// image loaded `bias` above its own addresses.
    #[must_use]
    pub fn addresses(&self, bias: u64) -> BTreeSet<u64> {
        self.intent(bias).into_values().flatten().collect()
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
    /// What the client saw at the last stop, which the hits since are
    /// judged against.
    pub baseline: RefCell<Option<Baseline>>,
    /// The watchpoints the debugger said it disabled, with the hits each
    /// had counted then, which it counts no more.
    pub disabled_watches: RefCell<BTreeMap<u64, (Intent, u64)>>,
    /// Whether the client disabled a watchpoint while the program ran
    /// since the last stop, freeing slots a thread that could not be
    /// armed may since have taken.
    pub released_while_running: Cell<bool>,
}

fn protocol(message: impl Into<String>) -> Failure {
    Failure::debugger("protocol", message)
}

impl Client {
    fn draw(&self, bound: u64) -> u64 {
        self.choices.borrow_mut().below(Stream::Client, bound)
    }

    /// A choice about enabling, temporary breakpoints, or advancing.
    fn control(&self, bound: u64) -> u64 {
        self.choices.borrow_mut().below(Stream::Control, bound)
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
        match result {
            Err(failure) if failure.check == "protocol" && self.ending(process) => {
                self.note(format!(
                    "the program is ending, so this may fail: {}",
                    failure.message
                ));
                Ok(())
            }
            result => result,
        }
    }

    /// Whether the program the client attached to began to end. A debugger
    /// whose attached program fails under it, as when killed from outside,
    /// detaches and exits.
    fn attached_program_ending(&self) -> bool {
        self.script
            .attach
            .is_some_and(|process| self.ending(process))
    }

    async fn snapshot(&self) -> Result<StateSnapshot, Failure> {
        self.handle
            .snapshot()
            .await
            .map_err(|error| protocol(format!("snapshot failed: {error}")))
    }

    /// The debugger's state, or `None` once it detached and exited.
    async fn state(&self) -> Result<Option<StateSnapshot>, Failure> {
        match self.handle.snapshot().await {
            Ok(snapshot) => Ok(Some(snapshot)),
            // A request sent just before the controller exited is dropped
            // unanswered.
            Err(Error::RequestQueueClosed | Error::RequestCancelled)
                if self.attached_program_ending() =>
            {
                self.note("the debugger detached and exited");
                Ok(None)
            }
            Err(error) => Err(protocol(format!("snapshot failed: {error}"))),
        }
    }

    /// Whether `process` began to end as a whole.
    fn ending(&self, process: ProcessId) -> bool {
        Tid::try_from(process.get()).is_ok_and(|tgid| self.shared.ending.borrow().contains(&tgid))
    }

    fn mark(&self, mark: Mark) {
        self.marks.borrow_mut().hit(mark);
    }

    fn observe(&self, observation: Observation) {
        self.shared.observations.borrow_mut().push(observation);
    }

    /// Runs the session to its end: requests, then a shutdown.
    pub async fn run(self) -> Result<(), Failure> {
        let mut events = self.handle.subscribe();
        self.load_views().await?;
        if self.script.follow {
            self.hold_forks().await?;
        }
        let mut breakpoints = Vec::new();
        for _ in 0..self.script.early_breakpoints {
            self.add_breakpoint(&mut breakpoints).await?;
        }
        let mut launches = 0;
        let mut watched_launch = 0;
        let mut last_stop: Option<StopId> = None;
        let mut stale: Option<StopId> = None;
        for _ in 0..self.script.requests {
            let Some(snapshot) = self.state().await? else {
                return Ok(());
            };
            match snapshot.inferior.clone() {
                InferiorState::NotRunning => {
                    if launches == self.script.launches {
                        break;
                    }
                    launches += 1;
                    if launches > 1 {
                        self.mark(Mark::Relaunched);
                    }
                    match self.script.attach.filter(|_| launches == 1) {
                        Some(process) => self.attach(process).await?,
                        None => self.launch().await?,
                    }
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
                        self.note_stop(stop_id, process_id, &reason)?;
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
                        self.judge_hits(process_id, &snapshot, &mut breakpoints)?;
                        // The semantic oracles judge every stop.
                        let result = self.inspect(stop_id).await;
                        self.excuse(process_id, result)?;
                        let released = self.released_while_running.replace(false);
                        if matches!(reason, StopReason::WatchpointArmFailed { .. }) && !released {
                            let result = self.resume_unarmed().await;
                            self.excuse(process_id, result)?;
                        }
                        // A watch from the first stop sees the whole run.
                        if self.script.watching && watched_launch != launches {
                            watched_launch = launches;
                            let result = self.add_watch().await;
                            self.excuse(process_id, result)?;
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

    /// Holds the children the program forks, for the world to hand to
    /// sessions of their own.
    async fn hold_forks(&self) -> Result<(), Failure> {
        let held = self
            .handle
            .hold_forks()
            .await
            .map_err(|error| protocol(format!("holding forks failed: {error}")))?;
        *self.shared.held.borrow_mut() = Some(held);
        self.shared.following.set(true);
        self.note("holding forks");
        Ok(())
    }

    /// Loads the views the program's types are presented with, if it has
    /// any.
    async fn load_views(&self) -> Result<(), Failure> {
        let Some(views) = &self.script.views else {
            return Ok(());
        };
        let errors = self
            .handle
            .load_views(&[("program.views", views)], &[])
            .await
            .map_err(|error| protocol(format!("loading views failed: {error}")))?;
        if errors.is_empty() {
            Ok(())
        } else {
            Err(protocol(format!(
                "the program's views have errors: {errors:?}"
            )))
        }
    }

    /// Marks what a new stop reports, failing on what no golden program
    /// does.
    fn note_stop(
        &self,
        stop_id: StopId,
        process_id: ProcessId,
        reason: &StopReason,
    ) -> Result<(), Failure> {
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
            StopReason::Watchpoint { .. } => self.mark(Mark::WatchpointStop),
            // Only slots others hold refuse a new thread.
            StopReason::WatchpointArmFailed { .. } => {
                if !matches!(self.script.debug, DebugBehavior::Contended(_)) {
                    return Err(protocol(format!("stop {stop_id} reports {reason:?}")));
                }
                self.mark(Mark::WatchArmFailed);
            }
            StopReason::ThreadExited { thread_id, .. } if thread_id.get() == process_id.get() => {
                self.mark(Mark::LeaderExitEndedExecution);
            }
            _ => {}
        }
        Ok(())
    }

    /// Attaches to the program running untraced, which may have begun to
    /// end before the client asked.
    async fn attach(&self, process: ProcessId) -> Result<(), Failure> {
        match self.handle.attach_process(process).await {
            Ok(stop) => {
                self.mark(Mark::Attached);
                self.note(format!("attached to {process} at stop {stop}"));
            }
            Err(error) if self.ending(process) => {
                self.note(format!("attaching to an ending program failed: {error}"));
            }
            Err(error) => return Err(protocol(format!("attaching failed: {error}"))),
        }
        Ok(())
    }

    async fn launch(&self) -> Result<(), Failure> {
        // A process's watchpoints end with it.
        self.shared.watches.borrow_mut().clear();
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
        if self.control(8) == 0 {
            if self.script.watching && self.control(2) == 0 {
                self.toggle_watch(true).await?;
            } else if self.toggle_breakpoint(breakpoints).await? {
                self.mark(Mark::EditWhileRunning);
            }
            return Ok(());
        }
        match self.draw(if self.script.watching { 8 } else { 7 }) {
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
            4 => self.amend_breakpoint(breakpoints).await?,
            7 => self.amend_watch().await?,
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
        if self.control(6) == 0 {
            match self.control(if self.script.watching { 4 } else { 3 }) {
                0 => {
                    self.toggle_breakpoint(breakpoints).await?;
                }
                1 => self.advance(breakpoints).await?,
                2 => self.jump_in_place().await?,
                _ => self.toggle_watch(false).await?,
            }
            return Ok(());
        }
        match self.draw(if self.script.watching { 21 } else { 16 }) {
            0 => {
                self.note("resume");
                match self.handle.resume().await {
                    Ok(reason) => self.note(format!("resumed until {reason:?}")),
                    Err(Error::EventStreamLagged(_)) => self.mark(Mark::ClientLagged),
                    Err(error) if self.refused_unarmed(&error).await? => {}
                    Err(error) => return Err(protocol(format!("resume failed: {error}"))),
                }
            }
            1 => {
                match self
                    .handle
                    .continue_execution(stop, scope, ExceptionDisposition::Pass)
                    .await
                {
                    Ok(execution) => {
                        self.note(format!("continued from {stop} as execution {execution}"));
                        self.wait_for(execution, events).await?;
                    }
                    Err(error) if self.refused_unarmed(&error).await? => {}
                    Err(error) => {
                        return Err(protocol(format!("continue from {stop} failed: {error}")));
                    }
                }
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
            11 => self.inspect(stop).await?,
            12 => self.amend_breakpoint(breakpoints).await?,
            13 | 16..=18 => self.add_watch().await?,
            14 | 19 => self.remove_watch().await?,
            20 => self.amend_watch().await?,
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
            .select_context(thread)
            .await
            .map_err(|error| protocol(format!("selecting thread {thread} failed: {error}")))?;
        self.note(format!("selected thread {thread}"));
        if snapshot.selected != Some(ExecutionContext::Thread(thread)) {
            self.mark(Mark::ThreadSelected);
        }
        Ok(())
    }

    /// Resumes one stopped thread alone. Its execution may never end by
    /// itself, so the client does not wait for it.
    async fn continue_thread(&self, snapshot: &StateSnapshot, stop: StopId) -> Result<(), Failure> {
        // Half the time the main thread, whose exit alone ends its
        // execution differently from any other thread's.
        let main = match snapshot.inferior {
            InferiorState::Stopped { process_id, .. } => snapshot
                .threads
                .iter()
                .find(|thread| thread.id.get() == process_id.get()),
            _ => None,
        };
        let thread = match main {
            Some(main) if self.draw(2) == 0 => main.id,
            _ => {
                self.choices
                    .borrow_mut()
                    .pick(Stream::Client, &snapshot.threads)
                    .id
            }
        };
        let execution = match self
            .handle
            .continue_execution(
                stop,
                ResumeScope::Thread(thread),
                ExceptionDisposition::Pass,
            )
            .await
        {
            Ok(execution) => execution,
            Err(error) if self.refused_unarmed(&error).await? => return Ok(()),
            Err(error) => {
                return Err(protocol(format!(
                    "continuing thread {thread} failed: {error}"
                )));
            }
        };
        self.note(format!(
            "continued thread {thread} alone as execution {execution}"
        ));
        self.mark(Mark::ThreadContinued);
        self.alone.set(Some(execution));
        Ok(())
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
        let snapshot = self.snapshot().await?;
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
        let before = self.snapshot().await?;
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
        let after = self.snapshot().await?;
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
                    let snapshot = self.snapshot().await?;
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
        let sent = self
            .handle
            .requests
            .send(ControllerMessage::Request(Request::Shutdown { reply }))
            .await;
        if sent.is_err() && self.attached_program_ending() {
            self.note("the debugger detached and exited");
            return Ok(());
        }
        sent.map_err(|_| protocol("the request queue closed before shutdown"))?;
        answer
            .await
            .map_err(|_| protocol("shutdown was never answered"))?
            .map_err(|error| protocol(format!("shutdown failed: {error}")))
    }
}
