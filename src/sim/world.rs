//! The world: one session of the real controller and client against the
//! simulated kernel, advanced one action at a time on one thread.
//!
//! Each step lists the enabled actions, lets the `Schedule` stream pick
//! one, performs it, records it in the trace, and runs the oracles:
//!
//! - `Run`: a running thread executes a burst of instructions.
//! - `Collect`: the waiter reaps a status and queues it for the controller.
//! - `Deliver`: the controller serves the message at the front of its queue.
//! - `Poll`: the client task runs until it waits again.
//!
//! The session ends when the client has shut the controller down and
//! nothing is left to do. A session in which nothing can happen while the
//! client still waits is stuck, which is a failure.

use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use tokio::sync::broadcast;

use super::choices::{Choices, Stream};
use super::client::{Client, Script};
use super::corpus::{Corpus, Run, Variant};
use super::kernel::{ExitStatus, Kernel, State, Tid, WaitStatus};
use super::marks::{Mark, Marks};
use super::oracles;
use super::report::{Failure, Trace};
use super::swarm::Swarm;
use crate::backend::sim_edge::{SimController, SimExecutable, SimLaunch};
use crate::{DebuggerEvent, DebuggerHandle};

/// The simulated debugger's process identifier.
const TRACER: i32 = 100;

/// How a run is carried out.
#[derive(Debug, Clone)]
pub struct Settings {
    /// How many trace lines to keep, or all of them.
    pub keep: Option<usize>,
    /// The most actions a session may take before it fails as runaway.
    pub max_steps: u64,
    /// Stops the run after this action, to inspect the state there.
    pub stop_at: Option<u64>,
    /// Breaks the simulation on purpose, to check that the oracles notice.
    #[cfg(test)]
    pub sabotage: Option<Sabotage>,
}

/// A deliberate defect, for tests of the oracles.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sabotage {
    /// Ptrace writes report success without writing.
    LosePokes,
    /// The waiter never reaps a status.
    DeafWaiter,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            keep: Some(400),
            max_steps: 2_000_000,
            stop_at: None,
            #[cfg(test)]
            sabotage: None,
        }
    }
}

/// What one run did.
pub struct Outcome {
    pub seed: u64,
    pub swarm: Swarm,
    /// The binary and arguments the session debugged.
    pub program: String,
    pub arguments: Vec<String>,
    pub fingerprint: u64,
    pub steps: u64,
    pub failure: Option<Failure>,
    /// The trace's kept lines, and how many older ones were dropped.
    pub trace: Vec<String>,
    pub dropped: u64,
    /// The state when the run stopped at `Settings::stop_at`.
    pub state: Option<String>,
    /// The interesting states the run reached.
    pub marks: Marks,
}

/// Runs the session `seed` names.
#[must_use]
pub fn run(seed: u64, corpus: &Corpus, settings: &Settings) -> Outcome {
    let mut choices = Choices::new(seed);
    let swarm = Swarm::choose(&mut choices, corpus);
    let program = &corpus.programs[swarm.program];
    let variant = &program.variants[swarm.variant];
    let run = &program.runs[swarm.run];
    let mut world = World::new(choices, swarm.clone(), program, variant, run, settings);
    let mut failure = None;
    let mut state = None;
    loop {
        match catch_panic(|| world.step()) {
            Ok(Ok(Progress::Continue)) => {}
            Ok(Ok(Progress::Finished)) => break,
            Ok(Err(mut found)) => {
                found.step = world.step;
                failure = Some(found);
                break;
            }
            Err(mut panic) => {
                panic.step = world.step;
                world.flush();
                failure = Some(panic);
                break;
            }
        }
        if settings.stop_at == Some(world.step) {
            state = Some(world.dump());
            break;
        }
        if world.step >= settings.max_steps {
            failure = Some(Failure {
                step: world.step,
                ..Failure::debugger(
                    "liveness",
                    format!("still going after {} actions", world.step),
                )
            });
            break;
        }
    }
    let (lines, dropped) = world.trace.lines();
    Outcome {
        seed,
        swarm,
        program: variant.name.clone(),
        arguments: run.arguments.clone(),
        fingerprint: world.trace.fingerprint(),
        steps: world.step,
        failure,
        trace: lines.iter().cloned().collect(),
        dropped,
        state,
        marks: world.marks.borrow().clone(),
    }
}

enum Progress {
    Continue,
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Run(Tid),
    Collect,
    Deliver,
    Poll,
}

/// Whether the client task asked to be polled again.
struct Woken(AtomicBool);

impl Wake for Woken {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Relaxed);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::Relaxed);
    }
}

type ClientTask = Pin<Box<dyn Future<Output = Result<(), Failure>>>>;

/// What the auditor has seen of the event stream.
#[derive(Default)]
struct Audit {
    /// The newest revision of any event.
    revision: u64,
    /// The newest revision a `StateChanged` announced.
    change: u64,
    stop: u64,
}

struct World<'a> {
    swarm: Swarm,
    variant: &'a Variant,
    run: &'a Run,
    choices: Rc<RefCell<Choices>>,
    kernel: Rc<RefCell<Kernel>>,
    /// Whether the controller started its waiter.
    waiter: Rc<Cell<bool>>,
    /// The controller, until it exits.
    controller: Option<SimController>,
    /// A status the waiter reaped but could not queue yet, the queue being
    /// full.
    hand: Option<WaitStatus>,
    /// The client task, until it finishes.
    client: Option<ClientTask>,
    woken: Arc<Woken>,
    waker: Waker,
    notes: Rc<RefCell<Vec<String>>>,
    marks: Rc<RefCell<Marks>>,
    auditor: broadcast::Receiver<DebuggerEvent>,
    audit: Audit,
    trace: Trace,
    step: u64,
    #[cfg(test)]
    sabotage: Option<Sabotage>,
    #[cfg(debug_assertions)]
    capture: crate::flight_recorder::Capture,
}

impl<'a> World<'a> {
    fn new(
        mut choices: Choices,
        swarm: Swarm,
        program: &'a super::corpus::Program,
        variant: &'a Variant,
        run: &'a Run,
        settings: &Settings,
    ) -> Self {
        #[cfg(debug_assertions)]
        let capture = crate::flight_recorder::Capture::start();
        let mut random = [0; 16];
        choices.fill(Stream::Program, &mut random);
        let choices = Rc::new(RefCell::new(choices));
        let kernel = Rc::new(RefCell::new(Kernel::new(TRACER)));
        #[cfg(test)]
        {
            kernel.borrow_mut().lose_pokes = settings.sabotage == Some(Sabotage::LosePokes);
        }
        let waiter = Rc::new(Cell::new(false));
        let debug_info = variant.debug_info();
        let module_image = Arc::clone(&debug_info.image);
        let (controller, channels) = SimController::new(
            Rc::clone(&kernel),
            Rc::clone(&waiter),
            SimLaunch {
                image: Arc::clone(&variant.image),
                path: Arc::clone(&variant.path),
                random,
            },
            &SimExecutable {
                path: Arc::clone(&variant.path),
                data: Arc::clone(&variant.data),
                inode: variant.inode,
            },
            debug_info,
            swarm.queue_capacity,
            swarm.event_capacity,
        );
        let handle = DebuggerHandle {
            module_image,
            core_dump: None,
            source_paths: Arc::default(),
            requests: channels.requests,
            events: channels.events,
        };
        let auditor = handle.subscribe();
        let notes = Rc::new(RefCell::new(Vec::new()));
        let marks = Rc::new(RefCell::new(Marks::default()));
        let client = Client {
            handle,
            choices: Rc::clone(&choices),
            notes: Rc::clone(&notes),
            marks: Rc::clone(&marks),
            script: Script {
                arguments: run.arguments.clone(),
                stop_at_entry: swarm.stop_at_entry,
                requests: swarm.requests,
                launches: swarm.launches,
                early_breakpoints: swarm.early_breakpoints,
                functions: program.functions.clone(),
                defined: variant.functions.clone(),
                source: program.source.clone(),
                source_lines: program.source_lines,
                image: Arc::clone(&variant.image),
            },
        };
        let woken = Arc::new(Woken(AtomicBool::new(true)));
        let waker = Waker::from(Arc::clone(&woken));
        let mut trace = Trace::new(settings.keep);
        trace.line(format!(
            "program {} {:?}; {swarm}",
            variant.name, run.arguments
        ));
        Self {
            swarm,
            variant,
            run,
            choices,
            kernel,
            waiter,
            controller: Some(controller),
            hand: None,
            client: Some(Box::pin(client.run())),
            woken,
            waker,
            notes,
            marks,
            auditor,
            audit: Audit::default(),
            trace,
            step: 0,
            #[cfg(test)]
            sabotage: settings.sabotage,
            #[cfg(debug_assertions)]
            capture,
        }
    }

    fn actions(&self) -> Vec<Action> {
        let kernel = self.kernel.borrow();
        let mut actions = kernel.runnable().map(Action::Run).collect::<Vec<_>>();
        if let Some(controller) = &self.controller {
            let collect = match self.hand {
                Some(_) => controller.has_room(),
                None => self.waiter.get() && kernel.reportable().next().is_some(),
            };
            #[cfg(test)]
            let collect = collect && self.sabotage != Some(Sabotage::DeafWaiter);
            if collect {
                actions.push(Action::Collect);
            }
            if controller.has_message() {
                actions.push(Action::Deliver);
            }
        }
        if self.client.is_some() && self.woken.0.load(Ordering::Relaxed) {
            actions.push(Action::Poll);
        }
        actions
    }

    /// Picks an action kind by the swarm's weights, then one action of
    /// that kind.
    fn choose(&self, actions: &[Action]) -> Action {
        let weights = self.swarm.weights;
        let kinds = [
            (
                weights.run,
                actions
                    .iter()
                    .any(|action| matches!(action, Action::Run(_))),
            ),
            (weights.collect, actions.contains(&Action::Collect)),
            (weights.deliver, actions.contains(&Action::Deliver)),
            (weights.poll, actions.contains(&Action::Poll)),
        ]
        .map(|(weight, enabled)| if enabled { weight } else { 0 });
        let mut choices = self.choices.borrow_mut();
        match choices.weighted(Stream::Schedule, &kinds) {
            0 => {
                let threads = actions
                    .iter()
                    .filter(|action| matches!(action, Action::Run(_)))
                    .copied()
                    .collect::<Vec<_>>();
                *choices.pick(Stream::Schedule, &threads)
            }
            1 => Action::Collect,
            2 => Action::Deliver,
            _ => Action::Poll,
        }
    }

    fn step(&mut self) -> Result<Progress, Failure> {
        let actions = self.actions();
        if actions.is_empty() {
            return self.finish();
        }
        let action = self.choose(&actions);
        self.step += 1;
        let line = self.perform(action)?;
        self.trace.line(format!("#{} {line}", self.step));
        self.flush();
        self.check()?;
        Ok(Progress::Continue)
    }

    /// Performs one action, returning its trace line.
    fn perform(&mut self, action: Action) -> Result<String, Failure> {
        match action {
            Action::Run(tid) => {
                let budget = self
                    .choices
                    .borrow_mut()
                    .below(Stream::Schedule, self.swarm.burst)
                    + 1;
                let mut kernel = self.kernel.borrow_mut();
                let executed = kernel.run(tid, budget);
                let state = kernel.threads.get(&tid).map(|thread| thread.state);
                Ok(format!(
                    "run {tid} x{executed} -> {}",
                    describe_state(state)
                ))
            }
            Action::Collect => {
                let controller = self
                    .controller
                    .as_ref()
                    .expect("collect needs a controller");
                let mut line = String::from("collect");
                if self.hand.is_none() {
                    let mut kernel = self.kernel.borrow_mut();
                    let ready = kernel.reportable().collect::<Vec<_>>();
                    let tid = *self.choices.borrow_mut().pick(Stream::Schedule, &ready);
                    let status = kernel.collect(tid).expect("a reportable thread reports");
                    let _ = write!(line, " {status}");
                    self.hand = Some(status);
                }
                let status = self.hand.take().expect("the waiter holds a status");
                if controller.queue_status(status) {
                    line.push_str(" -> queued");
                } else {
                    self.hand = Some(status);
                    self.marks.borrow_mut().hit(Mark::QueueFull);
                    line.push_str(" -> held, the queue is full");
                }
                Ok(line)
            }
            Action::Deliver => {
                let controller = self
                    .controller
                    .as_mut()
                    .expect("deliver needs a controller");
                let (description, running) = controller.deliver().expect("deliver needs a message");
                if running {
                    Ok(format!("deliver {description}"))
                } else {
                    self.controller = None;
                    Ok(format!("deliver {description} -> controller exited"))
                }
            }
            Action::Poll => {
                self.woken.0.store(false, Ordering::Relaxed);
                let task = self.client.as_mut().expect("poll needs a client");
                match task.as_mut().poll(&mut Context::from_waker(&self.waker)) {
                    Poll::Pending => Ok("poll client".to_owned()),
                    Poll::Ready(result) => {
                        self.client = None;
                        self.flush();
                        result.map(|()| "poll client -> done".to_owned())
                    }
                }
            }
        }
    }

    /// Moves what the controller recorded and what the client did into the
    /// trace.
    fn flush(&mut self) {
        #[cfg(debug_assertions)]
        for line in self.capture.take() {
            self.trace.line(format!("    | {line}"));
        }
        for note in self.notes.borrow_mut().drain(..) {
            self.trace.line(format!("    client: {note}"));
        }
    }

    /// The checks that run after every action.
    fn check(&mut self) -> Result<(), Failure> {
        if let Some(gap) = &self.kernel.borrow().gap {
            return Err(Failure::model_gap(gap.0.clone()));
        }
        self.audit()?;
        let kernel = self.kernel.borrow();
        if let Some(controller) = &self.controller {
            let truth = controller.truth();
            oracles::code_integrity(&kernel, &truth, &self.variant.image)
                .map_err(|message| Failure::debugger("code integrity", message))?;
            oracles::site_ownership(&truth)
                .map_err(|message| Failure::debugger("site ownership", message))?;
        }
        oracles::output_so_far(&kernel, self.run)
            .map_err(|message| Failure::debugger("transparency", message))?;
        Ok(())
    }

    /// Reads the events published since the last action: each state change
    /// announces a new revision, which the events it produced share, so
    /// revisions never decrease; stop identifiers only increase; and every
    /// exit the client hears of is the one the kernel saw.
    fn audit(&mut self) -> Result<(), Failure> {
        loop {
            let event = match self.auditor.try_recv() {
                Ok(event) => event,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => return Ok(()),
            };
            let revision = event.revision();
            if revision < self.audit.revision {
                return Err(Failure::debugger(
                    "events",
                    format!(
                        "revision {revision} follows {} in {event:?}",
                        self.audit.revision
                    ),
                ));
            }
            self.audit.revision = revision;
            match &event {
                DebuggerEvent::StateChanged { revision } => {
                    if *revision <= self.audit.change {
                        return Err(Failure::debugger(
                            "events",
                            format!(
                                "state change {revision} follows state change {}",
                                self.audit.change
                            ),
                        ));
                    }
                    self.audit.change = *revision;
                }
                DebuggerEvent::InferiorStopped { stop_id, .. } => {
                    if stop_id.get() <= self.audit.stop {
                        return Err(Failure::debugger(
                            "events",
                            format!("stop {stop_id} follows stop {}", self.audit.stop),
                        ));
                    }
                    self.audit.stop = stop_id.get();
                }
                DebuggerEvent::InferiorExited {
                    process_id, status, ..
                } => {
                    let tgid = Tid::try_from(process_id.get()).expect("a simulated pid fits");
                    let kernel = self.kernel.borrow();
                    let Some((truth, _)) = kernel.ended.get(&tgid) else {
                        return Err(Failure::debugger(
                            "events",
                            format!("process {tgid} reported {status:?} before it ended"),
                        ));
                    };
                    if matches!(truth, ExitStatus::Code(_)) {
                        self.marks.borrow_mut().hit(Mark::ProgramExited);
                    }
                    if !same_exit(*truth, status) {
                        return Err(Failure::debugger(
                            "events",
                            format!(
                                "process {tgid} ended {truth:?}, but the client heard {status:?}"
                            ),
                        ));
                    }
                }
                _ => {}
            }
        }
    }

    /// Ends a session in which nothing more can happen.
    fn finish(&self) -> Result<Progress, Failure> {
        if self.client.is_some() {
            return Err(Failure::debugger(
                "liveness",
                format!("the client waits but nothing can happen: {}", self.dump()),
            ));
        }
        if self.controller.is_some() {
            return Err(Failure::debugger(
                "liveness",
                "the controller kept running after it answered the shutdown",
            ));
        }
        let kernel = self.kernel.borrow();
        oracles::clean_exit(&kernel).map_err(|message| Failure::debugger("clean exit", message))?;
        oracles::transparency(&kernel, self.run)
            .map_err(|message| Failure::debugger("transparency", message))?;
        Ok(Progress::Finished)
    }

    /// The world's state, for a person to read.
    fn dump(&self) -> String {
        let kernel = self.kernel.borrow();
        let mut text = String::new();
        for thread in kernel.threads.values() {
            let _ = write!(
                text,
                "\n  thread {} of {}: {:?}, report {:?}, pending {:?}, rip {:#x}",
                thread.tid,
                thread.tgid,
                thread.state,
                thread.report,
                thread.pending,
                thread.registers.rip
            );
        }
        let _ = write!(
            text,
            "\n  waiter {}, holding {:?}; controller {}; client {}",
            if self.waiter.get() {
                "started"
            } else {
                "not started"
            },
            self.hand,
            match &self.controller {
                Some(controller) if controller.has_message() => "running, with messages queued",
                Some(_) => "running, queue empty",
                None => "exited",
            },
            if self.client.is_some() {
                "waiting"
            } else {
                "done"
            },
        );
        text
    }
}

fn describe_state(state: Option<State>) -> String {
    match state {
        None => "reaped".to_owned(),
        Some(State::Running) => "running".to_owned(),
        Some(State::Stopped { kind, .. }) => format!("stopped {kind:?}"),
        Some(State::Exiting(exit)) => format!("exiting {exit:?}"),
        Some(State::Zombie(exit)) => format!("zombie {exit:?}"),
    }
}

fn same_exit(kernel: ExitStatus, reported: &crate::ExitStatus) -> bool {
    match (kernel, reported) {
        (ExitStatus::Code(code), crate::ExitStatus::Code(reported)) => {
            i64::from(code & 0xff) == *reported
        }
        (ExitStatus::Signal(signal, _), crate::ExitStatus::Terminated(info)) => {
            u64::try_from(signal).is_ok_and(|signal| signal == info.code)
        }
        _ => false,
    }
}

thread_local! {
    /// The source file of this thread's last panic.
    static PANIC_FILE: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Runs `body`, turning a panic into a failure: the simulator's own when
/// its code panicked, the debugger's otherwise.
fn catch_panic<T>(body: impl FnOnce() -> T) -> Result<T, Failure> {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let file = info.location().map(|location| location.file().to_owned());
            PANIC_FILE.with_borrow_mut(|last| *last = file);
            previous(info);
        }));
    });
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).map_err(|payload| {
        let message = payload
            .downcast_ref::<&str>()
            .map(|message| (*message).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a panic without a message".to_owned());
        let file = PANIC_FILE.with_borrow_mut(Option::take).unwrap_or_default();
        let message = format!("panicked at {file}: {message}");
        if file.contains("src/sim/") || file.ends_with("sim_edge.rs") {
            Failure::simulator("panic", message)
        } else {
            Failure::debugger("panic", message)
        }
    })
}
