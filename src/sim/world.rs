//! The world: one session of the real controller and client against the
//! simulated kernel, advanced one action at a time on one thread.
//!
//! Each step lists the enabled actions, lets the scheduler pick one,
//! performs it, records it in the trace, and runs the oracles:
//!
//! - `Run`: a running thread executes a burst of instructions.
//! - `Collect`: the waiter reaps a status and queues it for the controller.
//! - `Deliver`: the controller serves the message at the front of its queue.
//! - `Poll`: the client task runs until it waits again.
//!
//! Inside `Deliver`, every call the controller makes into the kernel is a
//! preemption point, where threads may run and the waiter may reap before
//! the call takes effect, as on Linux. A planned fault fires as an action of
//! its own, or at a preemption point.
//!
//! The session ends when the client has shut the controller down and
//! nothing is left to do. A session in which nothing can happen while the
//! client still waits is stuck, which is a failure.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use nix::libc;

use super::audit::Auditor;
use super::choices::{Choices, Stream};
use super::client::{Client, Observation, Script, Shared};
use super::corpus::{Corpus, Program, Run, Variant};
use super::faults::{Faults, Plan};
use super::kernel::shadow::Tracking;
use super::kernel::watching::UserWatch;
use super::kernel::{Kernel, Parent, State, StopKind, Tid};
use super::machine::Machine;
use super::marks::{Mark, Marks};
use super::oracles::{self, HeardTrap};
use super::report::{Failure, Trace};
use super::schedule::{Action, Scheduler};
use super::semantics::{self, Begun, Inspected, Judged, Unwound};
use super::swarm::Swarm;
use super::watches::{self, Intent};
use crate::backend::sim_edge::{
    Preemption, SimController, SimExecutable, SimLaunch, SimParts, SimWaiter,
};
use crate::{DebuggerHandle, ProcessId};

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
    /// Once a stop is published, a stopped thread runs again behind the
    /// controller's back.
    ResumeBehindTheController,
    /// The CPU executes the program's own instruction under a trap the
    /// debugger planted for the user.
    SkipTraps,
    /// Ptrace reads of a return address where a call pushed it report the
    /// next address.
    SkewReturnAddresses,
    /// A single step of the thread the client steps executes two
    /// instructions.
    LateSingleSteps,
    /// Ptrace reads of small numbers other than zero on the main thread's
    /// stack report them one greater. Zeros, which end chains of frames,
    /// and slots where calls pushed return addresses stay as they are.
    SkewSmallStackWords,
    /// Ptrace writes to the debug registers of threads other than a
    /// process's first go to a copy that reads them back, leaving the
    /// thread's own slots as they were.
    PhantomArming,
    /// Threads other than a process's first take no debug exception for
    /// an access their slots cover.
    MissWatchTraps,
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
    /// The planned fault, if it never fired.
    pub unfired: Option<Plan>,
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
        match catch_panic(|| world.step_once()) {
            Ok(Ok(Progress::Continue)) => {}
            Ok(Ok(Progress::Finished)) => break,
            Ok(Err(mut found)) => {
                found.step = world.step();
                failure = Some(found);
                break;
            }
            Err(mut panic) => {
                panic.step = world.step();
                world.flush();
                failure = Some(panic);
                break;
            }
        }
        if settings.stop_at == Some(world.step()) {
            state = Some(world.dump());
            break;
        }
        if world.step() >= settings.max_steps {
            failure = Some(Failure {
                step: world.step(),
                ..Failure::debugger(
                    "liveness",
                    format!("still going after {} actions", world.step()),
                )
            });
            break;
        }
    }
    let (lines, dropped) = world.trace.lines();
    let faults = world.machine.faults.borrow();
    Outcome {
        seed,
        swarm,
        program: variant.name.clone(),
        arguments: run.arguments.clone(),
        fingerprint: world.trace.fingerprint(),
        steps: world.step(),
        failure,
        trace: lines.iter().cloned().collect(),
        dropped,
        state,
        marks: world.machine.marks.borrow().clone(),
        unfired: faults.plan.filter(|_| faults.fired.is_none()),
    }
}

enum Progress {
    Continue,
    Finished,
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

struct World<'a> {
    swarm: Swarm,
    program: &'a Program,
    variant: &'a Variant,
    run: &'a Run,
    machine: Rc<Machine>,
    /// The controller, until it exits.
    controller: Option<SimController>,
    /// The client task, until it finishes.
    client: Option<ClientTask>,
    shared: Shared,
    woken: Arc<Woken>,
    waker: Waker,
    auditor: Auditor,
    /// Each thread's latest arrival at a trap the controller heard of.
    arrivals: BTreeMap<Tid, Arrival>,
    /// Whether the controller was asked to shut down.
    shutting_down: bool,
    /// The step the client requested, until it ends.
    stepping: Option<Begun>,
    /// The watchpoints the client knows of, with the bytes each watched at
    /// the last stop.
    watches: Vec<Intent>,
    baselines: BTreeMap<u64, Vec<u8>>,
    /// The last stop watch accounting judged.
    judged_stop: Option<u64>,
    trace: Trace,
    #[cfg(debug_assertions)]
    capture: crate::flight_recorder::Capture,
}

impl<'a> World<'a> {
    fn new(
        mut choices: Choices,
        swarm: Swarm,
        program: &'a Program,
        variant: &'a Variant,
        run: &'a Run,
        settings: &Settings,
    ) -> Self {
        #[cfg(debug_assertions)]
        let capture = crate::flight_recorder::Capture::start();
        let mut random = [0; 16];
        choices.fill(Stream::Program, &mut random);
        let scheduler = Scheduler::new(swarm.policy, &mut choices);
        let choices = Rc::new(RefCell::new(choices));
        let kernel = Rc::new(RefCell::new(Kernel::new(TRACER)));
        kernel.borrow_mut().debug_behavior = swarm.debug;
        #[cfg(test)]
        {
            let mut kernel = kernel.borrow_mut();
            kernel.sabotage = settings.sabotage;
        }
        let marks = Rc::new(RefCell::new(Marks::default()));
        let shared = Shared::default();
        let attach = swarm
            .attach
            .map(|_| start_untraced(&mut kernel.borrow_mut(), variant, run, random));
        let machine = Rc::new(Machine {
            kernel: Rc::clone(&kernel),
            waiter: Rc::new(RefCell::new(SimWaiter::default())),
            choices: Rc::clone(&choices),
            scheduler: RefCell::new(scheduler),
            faults: RefCell::new(Faults::new(swarm.fault)),
            marks: Rc::clone(&marks),
            killed: Rc::clone(&shared.killed),
            ending: Rc::clone(&shared.ending),
            unclean: RefCell::new(Vec::new()),
            step: Cell::new(0),
            ran: Cell::new(0),
            preempt: swarm.preempt,
            #[cfg(test)]
            sabotage: settings.sabotage,
        });
        let debug_info = variant.debug_info();
        let module_image = Arc::clone(&debug_info.image);
        let (controller, channels) = SimController::new(SimParts {
            kernel,
            waiter: Rc::clone(&machine.waiter),
            preemption: Rc::clone(&machine) as Rc<dyn Preemption>,
            launch: SimLaunch {
                image: Arc::clone(&variant.image),
                path: Arc::clone(&variant.path),
                random,
            },
            executable: SimExecutable {
                path: Arc::clone(&variant.path),
                data: Arc::clone(&variant.data),
                inode: variant.inode,
            },
            debug_info,
            queue_capacity: swarm.queue_capacity,
            event_capacity: swarm.event_capacity,
        });
        let handle = DebuggerHandle {
            module_image,
            core_dump: None,
            source_paths: Arc::default(),
            requests: channels.requests,
            events: channels.events,
        };
        let auditor = Auditor::new(handle.subscribe(), Rc::clone(&shared.published));
        let client = Client {
            handle,
            choices,
            marks,
            shared: shared.clone(),
            script: script(&swarm, program, variant, run, attach),
            alone: Cell::new(None),
            baseline: RefCell::new(None),
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
            program,
            variant,
            run,
            machine,
            controller: Some(controller),
            client: Some(Box::pin(client.run())),
            shared,
            woken,
            waker,
            auditor,
            arrivals: BTreeMap::new(),
            shutting_down: false,
            stepping: None,
            watches: Vec::new(),
            baselines: BTreeMap::new(),
            judged_stop: None,
            trace,
            #[cfg(debug_assertions)]
            capture,
        }
    }

    fn step(&self) -> u64 {
        self.machine.step.get()
    }

    /// Whether the client may act: a program started untraced first runs
    /// as many instructions as the swarm says, unless it ends sooner.
    fn client_may_act(&self) -> bool {
        let Some(after) = self.swarm.attach else {
            return true;
        };
        self.machine.ran.get() >= after
            || self
                .machine
                .kernel
                .borrow()
                .processes
                .values()
                .all(|process| process.parent != Parent::Launcher)
    }

    fn actions(&self) -> Vec<Action> {
        let mut actions = self
            .machine
            .kernel
            .borrow()
            .runnable()
            .map(Action::Run)
            .collect::<Vec<_>>();
        if let Some(controller) = &self.controller {
            if self.machine.can_collect() {
                actions.push(Action::Collect);
            }
            if controller.has_message() {
                actions.push(Action::Deliver);
            }
        }
        if self.client.is_some() && self.woken.0.load(Ordering::Relaxed) && self.client_may_act() {
            actions.push(Action::Poll);
        }
        actions
    }

    fn step_once(&mut self) -> Result<Progress, Failure> {
        let step = self.step() + 1;
        let fault_due = self.machine.faults.borrow().at_step(step);
        if fault_due && let Some(line) = self.machine.kill(Mark::KilledAtStep) {
            self.machine.step.set(step);
            self.trace.line(format!("#{step} {line}"));
            self.check()?;
            return Ok(Progress::Continue);
        }
        let actions = self.actions();
        if actions.is_empty() {
            return self.finish();
        }
        let action = self.machine.scheduler.borrow_mut().choose(
            &actions,
            step,
            &mut self.machine.choices.borrow_mut(),
        );
        self.machine.step.set(step);
        let performed = self.perform(action);
        let line = match &performed {
            Ok(line) => line.clone(),
            Err(failure) => format!("{action:?} failed: {}", failure.check),
        };
        self.trace.line(format!("#{step} {line}"));
        for line in self.machine.absorb() {
            self.trace.line(format!("    {line}"));
        }
        self.flush();
        performed?;
        if action == Action::Poll {
            // The user's breakpoints, as the client now knows them, are what
            // the kernel watches for unseen hits.
            self.machine.kernel.borrow_mut().user_breakpoints =
                self.shared.addresses(self.variant.image.bias());
            self.follow_watches();
            self.judge()?;
        }
        #[cfg(test)]
        self.sabotage();
        self.check()?;
        Ok(Progress::Continue)
    }

    /// Resumes a stopped thread behind the controller's back while a stop
    /// is published, under [`Sabotage::ResumeBehindTheController`].
    #[cfg(test)]
    fn sabotage(&self) {
        if self.machine.sabotage != Some(Sabotage::ResumeBehindTheController)
            || self
                .controller
                .as_ref()
                .is_none_or(|controller| controller.truth().public_stop.is_none())
        {
            return;
        }
        let mut kernel = self.machine.kernel.borrow_mut();
        if let Some(thread) = kernel.threads.values_mut().find(|thread| {
            matches!(
                thread.state,
                State::Stopped {
                    kind: StopKind::Signal(_),
                    ..
                }
            )
        }) {
            thread.state = State::Running;
            thread.report = None;
        }
    }

    /// Performs one action, returning its trace line.
    fn perform(&mut self, action: Action) -> Result<String, Failure> {
        match action {
            Action::Run(tid) => {
                let budget = self
                    .machine
                    .choices
                    .borrow_mut()
                    .below(Stream::Schedule, self.swarm.burst)
                    + 1;
                Ok(self.machine.run_thread(tid, budget))
            }
            Action::Collect => Ok(self.machine.collect()),
            Action::Deliver => self.deliver(),
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

    /// Lets the controller handle the message at the front of its queue,
    /// and checks the breakpoint hits it counted.
    fn deliver(&mut self) -> Result<String, Failure> {
        let controller = self
            .controller
            .as_mut()
            .expect("deliver needs a controller");
        let delivery = controller.take().expect("deliver needs a message");
        let description = delivery.description.clone();
        let before = controller.truth();
        // A trap whose stop the controller now hears of: each arrival at a
        // trap is one hit.
        let trapped = delivery
            .trap
            .and_then(|tid| trap_of(&self.machine.kernel.borrow(), tid));
        let trap = trapped.map(|(tid, address, retired)| HeardTrap {
            tid,
            address,
            again: self
                .arrivals
                .get(&tid)
                .filter(|arrival| arrival.address == address && arrival.retired == retired)
                .map(|arrival| arrival.counted.clone()),
            in_shutdown: self.shutting_down,
        });
        self.shutting_down |= delivery.shutdown;
        let running = controller.handle(delivery);
        if !running {
            self.controller = None;
            let mut kernel = self.machine.kernel.borrow_mut();
            if let Some(thread) = kernel
                .threads
                .values()
                .find(|thread| thread.traced() && thread.held())
            {
                return Err(Failure::debugger(
                    "clean exit",
                    format!(
                        "the controller exited holding thread {} in a stop",
                        thread.tid
                    ),
                ));
            }
            // The tracer thread exits with the controller, which releases
            // what it could not detach (K-WAIT-3).
            kernel.forget_tracer();
            return Ok(format!("deliver {description} -> controller exited"));
        }
        let after = controller.truth();
        // A group exit or SIGKILL meanwhile takes the trap's thread out of
        // its stop, and the controller may never count the hit.
        let disturbed = trap.as_ref().is_some_and(|trap| {
            self.machine
                .kernel
                .borrow()
                .process_of(trap.tid)
                .is_none_or(|process| process.group_exit.is_some())
        });
        oracles::hit_counts(&before, &after, trap.as_ref(), disturbed)
            .map_err(|message| Failure::debugger("breakpoint accounting", message))?;
        if let Some((tid, address, retired)) = trapped {
            let counted = after.breakpoints.iter().filter_map(|(&id, breakpoint)| {
                let previous = before
                    .breakpoints
                    .get(&id)
                    .map_or(0, |earlier| earlier.hit_count);
                (breakpoint.hit_count > previous).then_some(id)
            });
            let arrival = self.arrivals.entry(tid).or_default();
            if (arrival.address, arrival.retired) != (address, retired) {
                *arrival = Arrival {
                    address,
                    retired,
                    counted: BTreeSet::new(),
                };
            }
            arrival.counted.extend(counted);
        }
        Ok(format!("deliver {description}"))
    }

    /// Judges what the client saw since the last poll by the semantic
    /// oracles. Nothing the client saw at a stop is judged once the stop is
    /// over, or while its process is ending, which moves its threads.
    fn judge(&mut self) -> Result<(), Failure> {
        let observations = std::mem::take(&mut *self.shared.observations.borrow_mut());
        for observation in observations {
            match observation {
                Observation::Backtrace { stop, backtrace } if self.still_at(stop) => {
                    let unwound = semantics::backtrace(&self.machine.kernel.borrow(), &backtrace)
                        .map_err(|message| Failure::debugger("backtrace", message))?;
                    let mark = match unwound {
                        Some(Unwound::Whole) => Mark::WholeBacktrace,
                        Some(Unwound::Truncated) => Mark::TruncatedBacktrace,
                        Some(Unwound::Corrupt) => Mark::CorruptCaller,
                        None => continue,
                    };
                    self.machine.marks.borrow_mut().hit(mark);
                }
                Observation::StepBegins {
                    thread,
                    kind,
                    presentation,
                } => self.begin_step(thread, kind, presentation.as_ref()),
                Observation::StepEnded(reason) => self.judge_step(reason.as_ref())?,
                Observation::Variables {
                    stop,
                    variables,
                    backtrace,
                } if self.still_at(stop) => {
                    let source = self
                        .program
                        .source
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or_default();
                    let inspected = semantics::variables(
                        &self.machine.kernel.borrow(),
                        &variables,
                        &backtrace,
                        self.variant,
                        source,
                        &self.program.markers,
                    )
                    .map_err(|message| Failure::debugger("variables", message))?;
                    if inspected == Some(Inspected::Held) {
                        self.machine.marks.borrow_mut().hit(Mark::MarkerHeld);
                    }
                }
                Observation::Backtrace { .. } | Observation::Variables { .. } => {}
            }
        }
        Ok(())
    }

    /// Follows the watchpoints the client now knows of: the kernel watches
    /// their accesses, and a new one's bytes, the program stopped, are its
    /// baseline.
    fn follow_watches(&mut self) {
        let watches = self
            .shared
            .watches
            .borrow()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        if watches == self.watches {
            return;
        }
        let mut kernel = self.machine.kernel.borrow_mut();
        let tgid = self
            .controller
            .as_ref()
            .and_then(|controller| controller.truth().inferior);
        self.baselines
            .retain(|id, _| watches.iter().any(|watch| watch.id == *id));
        for watch in &watches {
            if !self.baselines.contains_key(&watch.id)
                && let Some(bytes) = tgid.and_then(|tgid| watches::bytes(&kernel, tgid, watch))
            {
                self.baselines.insert(watch.id, bytes);
            }
        }
        kernel.watching.watches = watches
            .iter()
            .map(|watch| UserWatch {
                id: watch.id,
                spans: watch.spans.clone(),
                loads: matches!(
                    watch.access,
                    crate::WatchAccess::ReadWrite | crate::WatchAccess::Read
                ),
                every: watch.access != crate::WatchAccess::Change,
            })
            .collect();
        drop(kernel);
        self.watches = watches;
    }

    /// Watch accounting, at each new stop: the hits reported against the
    /// accesses made since the last. Every stop starts the accesses and
    /// baselines again.
    fn account_watches(&mut self, truth: &crate::backend::sim_edge::Truth) -> Result<(), Failure> {
        let (Some(stop), Some(tgid)) = (truth.public_stop, truth.inferior) else {
            return Ok(());
        };
        if self.judged_stop == Some(stop) {
            return Ok(());
        }
        self.judged_stop = Some(stop);
        let mut kernel = self.machine.kernel.borrow_mut();
        let ending = kernel
            .processes
            .get(&tgid)
            .is_none_or(|process| process.group_exit.is_some());
        if !ending {
            let found = watches::judge(
                &kernel,
                tgid,
                &truth.reasons,
                &self.watches,
                &self.baselines,
            )
            .map_err(|message| Failure::debugger("watch accounting", message))?;
            let mut marks = self.machine.marks.borrow_mut();
            if found.hit {
                marks.hit(Mark::WatchHit);
            }
            if found.other_thread {
                marks.hit(Mark::WatchHitOnAnotherThread);
            }
            if found.unchanged {
                marks.hit(Mark::UnchangedStore);
            }
        }
        kernel.watching.restart();
        for watch in &self.watches {
            if let Some(bytes) = watches::bytes(&kernel, tgid, watch) {
                self.baselines.insert(watch.id, bytes);
            }
        }
        Ok(())
    }

    /// Whether `stop` is still published, and its process is not ending.
    fn still_at(&self, stop: crate::StopId) -> bool {
        let Some(truth) = self.controller.as_ref().map(SimController::truth) else {
            return false;
        };
        truth.public_stop == Some(stop.get())
            && truth.inferior.is_some_and(|tgid| {
                self.machine
                    .kernel
                    .borrow()
                    .processes
                    .get(&tgid)
                    .is_some_and(|process| process.group_exit.is_none())
            })
    }

    /// Notes where a step the client is about to request begins, and starts
    /// recording where its thread goes.
    fn begin_step(
        &mut self,
        thread: crate::ThreadId,
        kind: crate::StepKind,
        presentation: Option<&crate::FramePresentation>,
    ) {
        let mut kernel = self.machine.kernel.borrow_mut();
        let tid = Tid::try_from(thread.get()).expect("a simulated tid fits");
        let Some(stepped) = kernel.threads.get(&tid) else {
            return;
        };
        let begun = Begun::new(stepped, kind, presentation);
        kernel.tracking = Some(Tracking {
            tid,
            depth: begun.shadow.depth(),
            positions: Vec::new(),
        });
        self.stepping = Some(begun);
    }

    /// Judges where a step ended, if it ended as the step it was.
    fn judge_step(&mut self, reason: Option<&crate::StopReason>) -> Result<(), Failure> {
        let tracking = self.machine.kernel.borrow_mut().tracking.take();
        let begun = self.stepping.take();
        let (Some(begun), Some(tracking)) = (begun, tracking) else {
            return Ok(());
        };
        let stop = self
            .controller
            .as_ref()
            .and_then(|controller| controller.truth().public_stop);
        let ended = matches!(reason, Some(crate::StopReason::Step { kind }) if *kind == begun.kind);
        if !ended || !stop.is_some_and(|stop| self.still_at(crate::StopId::new(stop))) {
            return Ok(());
        }
        let judged = semantics::step(
            &self.machine.kernel.borrow(),
            &begun,
            &tracking.positions,
            self.variant,
        )
        .map_err(|message| Failure::debugger("stepping", message))?;
        let mut marks = self.machine.marks.borrow_mut();
        if judged.is_some() {
            marks.hit(Mark::StepJudged);
        }
        if judged == Some(Judged::Exactly) {
            marks.hit(Mark::SourceStepExact);
        }
        Ok(())
    }

    /// Moves what the controller recorded and what the client did into the
    /// trace.
    fn flush(&mut self) {
        #[cfg(debug_assertions)]
        for line in self.capture.take() {
            self.trace.line(format!("    | {line}"));
        }
        for note in self.shared.notes.borrow_mut().drain(..) {
            self.trace.line(format!("    client: {note}"));
        }
    }

    /// The checks that run after every action.
    fn check(&mut self) -> Result<(), Failure> {
        if let Some(gap) = &self.machine.kernel.borrow().gap {
            return Err(Failure::model_gap(gap.0.clone()));
        }
        self.auditor.check(
            &self.machine.kernel.borrow(),
            &mut self.machine.marks.borrow_mut(),
        )?;
        if let Some(unseen) = self.machine.kernel.borrow().watching.unseen.first() {
            return Err(Failure::debugger(
                "watch accounting",
                format!(
                    "thread {} accessed {:#x}, which watchpoint {} watches, with no slot armed for it",
                    unseen.tid, unseen.address, unseen.watch
                ),
            ));
        }
        if let Some(&(tid, (address, now, was))) = self.machine.unclean.borrow().first() {
            return Err(Failure::debugger(
                "clean release",
                format!(
                    "thread {tid} was released with {now:#04x} at {address:#x}, where the \
                     program has {was:#04x}"
                ),
            ));
        }
        if let Some(lost) = self.machine.kernel.borrow().watching.lost.first() {
            return Err(Failure::debugger(
                "watch accounting",
                format!(
                    "thread {} ran on from an access to watchpoint {} that no stop reported",
                    lost.tid, lost.watch
                ),
            ));
        }
        // Only a process the controller debugs has the user's breakpoints
        // and watches, once it finished launching or attaching: none once it
        // detached, or once it exited itself.
        let truth = self.controller.as_ref().map(SimController::truth);
        let debugged = truth
            .as_ref()
            .filter(|truth| truth.established)
            .and_then(|truth| truth.inferior);
        let mut kernel = self.machine.kernel.borrow_mut();
        if kernel.debugged != debugged {
            // What a process's threads accessed is the debugger's to report
            // only while it debugs the process.
            kernel.watching.restart();
            kernel.debugged = debugged;
        }
        drop(kernel);
        if let Some(truth) = truth {
            self.account_watches(&truth)?;
        }
        let kernel = self.machine.kernel.borrow();
        oracles::unseen_hits(&kernel)
            .map_err(|message| Failure::debugger("breakpoint accounting", message))?;
        if let Some(controller) = &self.controller {
            let truth = controller.truth();
            oracles::code_integrity(&kernel, &truth, &self.variant.image)
                .map_err(|message| Failure::debugger("code integrity", message))?;
            oracles::site_ownership(&kernel, &truth)
                .map_err(|message| Failure::debugger("site ownership", message))?;
            oracles::all_stop(&kernel, &truth)
                .map_err(|message| Failure::debugger("all-stop", message))?;
            oracles::user_breakpoints(
                &kernel,
                &truth,
                &self.shared.intent(self.variant.image.bias()),
            )
            .map_err(|message| Failure::debugger("breakpoint accounting", message))?;
        }
        oracles::output_so_far(&kernel, self.run)
            .map_err(|message| Failure::debugger("transparency", message))?;
        Ok(())
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
        let kernel = self.machine.kernel.borrow();
        oracles::clean_exit(&kernel).map_err(|message| Failure::debugger("clean exit", message))?;
        oracles::transparency(&kernel, self.run)
            .map_err(|message| Failure::debugger("transparency", message))?;
        let mut marks = self.machine.marks.borrow_mut();
        let ran_on = kernel
            .ended
            .values()
            .any(|ended| ended.reaper == Parent::Launcher && !ended.killed_externally);
        if marks.count(Mark::Detached) > 0 && ran_on {
            marks.hit(Mark::FinishedAfterDetach);
        }
        Ok(Progress::Finished)
    }

    /// The world's state, for a person to read.
    fn dump(&self) -> String {
        let kernel = self.machine.kernel.borrow();
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
        let waiter = self.machine.waiter.borrow();
        let _ = write!(
            text,
            "\n  waiter {}, holding {:?}; controller {}; client {}",
            if waiter.started() {
                "started"
            } else {
                "not started"
            },
            waiter.holding(),
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

/// What the client knows of the program it debugs.
/// Starts the program untraced, for the client to attach to.
fn start_untraced(
    kernel: &mut Kernel,
    variant: &Variant,
    run: &Run,
    random: [u8; 16],
) -> ProcessId {
    let tgid = kernel.spawn_untraced(
        Arc::clone(&variant.image),
        &variant.path,
        &run.arguments,
        random,
    );
    ProcessId::new(u64::try_from(tgid).expect("process ids are positive"))
}

fn script(
    swarm: &Swarm,
    program: &Program,
    variant: &Variant,
    run: &Run,
    attach: Option<ProcessId>,
) -> Script {
    Script {
        arguments: run.arguments.clone(),
        stop_at_entry: swarm.stop_at_entry,
        requests: swarm.requests,
        launches: swarm.launches,
        early_breakpoints: swarm.early_breakpoints,
        watching: swarm.watching,
        functions: program.functions.clone(),
        defined: variant.functions.clone(),
        source: program.source.clone(),
        source_lines: program.source_lines,
        marker_lines: program.markers.iter().map(|marker| marker.line).collect(),
        markers: program
            .markers
            .iter()
            .map(|marker| (marker.line, marker.text.clone()))
            .collect(),
        marker_rows: marker_rows(program, variant),
        globals: variant.globals.clone(),
        debug: swarm.debug,
        image: Arc::clone(&variant.image),
        attach,
    }
}

/// Where, in unoptimized code, a row of a marker's line starts, with the
/// line, by image address.
fn marker_rows(program: &Program, variant: &Variant) -> std::collections::BTreeMap<u64, u64> {
    let facts = &variant.facts;
    let source = program
        .source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if facts.optimized {
        return std::collections::BTreeMap::new();
    }
    program
        .markers
        .iter()
        .flat_map(|marker| {
            facts
                .line_starts(source, marker.line)
                .map(move |address| (address, marker.line))
        })
        .collect()
}

/// A thread's arrival at a trap: where, how many instructions it had
/// completed, and the breakpoints that counted it.
#[derive(Default)]
struct Arrival {
    address: u64,
    retired: u64,
    counted: BTreeSet<u64>,
}

/// The trap whose stop `tid` is in, if its stop is one: the thread, where,
/// and how many instructions it had completed then.
fn trap_of(kernel: &Kernel, tid: Tid) -> Option<(Tid, u64, u64)> {
    let thread = kernel.threads.get(&tid)?;
    match thread.state {
        State::Stopped {
            kind: StopKind::Signal(libc::SIGTRAP),
            info,
        } if info.code == super::cpu::SI_KERNEL && thread.trapped_at.is_some() => thread
            .last_trap
            .map(|(address, retired)| (tid, address, retired)),
        _ => None,
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
