//! The world: sessions of the real controller and client against the
//! simulated kernel, advanced one action at a time on one thread.
//!
//! The first session is the one the client drives and the oracles judge in
//! full. When it holds the children its program forks, each child it holds
//! is adopted by a session of its own, as a DAP client starts one for each
//! `startDebugging` request, or released. Each session's controller traces
//! as a process of its own, with a waiter of its own.
//!
//! Each step lists the enabled actions, lets the scheduler pick one,
//! performs it, records it in the trace, and runs the oracles:
//!
//! - `Run`: a running thread executes a burst of instructions.
//! - `Collect`: a session's waiter reaps a status and queues it for its
//!   controller.
//! - `Deliver`: a session's controller serves the message at the front of
//!   its queue.
//! - `Poll`: a session's client task runs until it waits again.
//! - `Follow`: the next child the first session held is adopted or
//!   released.
//!
//! Inside `Deliver`, every call the controller makes into the kernel is a
//! preemption point, where threads may run and waiters may reap before the
//! call takes effect, as on Linux. A planned fault fires as an action of
//! its own, or at a preemption point.
//!
//! The run ends when every client has shut its controller down and nothing
//! is left to do. A run in which nothing can happen while a client still
//! waits is stuck, which is a failure.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
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
use super::client::{Adopter, Client, Observation, Script, Shared};
use super::corpus::{Corpus, Program, Run, Variant};
use super::faults::{Faults, Plan};
use super::kernel::shadow::Tracking;
use super::kernel::watching::UserWatch;
use super::kernel::{Kernel, Parent, State, StopKind, Tid};
use super::machine::Machine;
use super::marks::{Mark, Marks};
use super::oracles::{self, HeardTrap};
use super::report::{Failure, Trace};
use super::schedule::{Action, Scheduler, SessionId};
use super::semantics::{self, Begun, Inspected, Judged, Unwound};
use super::swarm::Swarm;
use super::views;
use super::watches::{self, Intent};
use crate::backend::sim_edge::{
    Preemption, SimController, SimExecutable, SimLaunch, SimParts, SimWaiter,
};
use crate::{DebuggerHandle, HeldChild, HeldProcess, ProcessId};

/// The process identifier of the first session's debugger, which launches
/// programs. Each session adopting a held child traces as the next.
pub const TRACER: i32 = 100;
/// The session the client drives.
const FIRST: SessionId = 0;

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
    /// Ptrace reads of the same small numbers report every other one one
    /// greater, so two reads of a value that has not changed disagree.
    FlickeringStackWords,
    /// `PTRACE_GETREGS` reports general registers other than the stack and
    /// frame pointers one greater when they hold small numbers other than
    /// zero, and `PTRACE_SETREGS` takes back what it reported, so the
    /// program runs on unchanged.
    SkewSmallRegisters,
    /// Ptrace writes to the debug registers of threads other than a
    /// process's first go to a copy that reads them back, leaving the
    /// thread's own slots as they were.
    PhantomArming,
    /// Threads other than a process's first take no debug exception for
    /// an access their slots cover.
    MissWatchTraps,
    /// A thread's debug exception for an access its slots cover is raised
    /// again after its next instruction, which accessed nothing.
    RepeatWatchTraps,
    /// Ptrace reads of a word that points eight bytes past itself, as a
    /// linked node's next link does when its successor follows it in
    /// memory, report the node after that successor, so a list walk skips
    /// a node.
    SkipLinkedNodes,
    /// `PTRACE_DETACH` forgets a SIGSTOP pending for the thread, so a fork
    /// child released to be held runs on instead.
    ForgetStopRequests,
    /// The kernel never reports a SIGCONT pending, so a session detaching
    /// from a held child leaves the one that ended its stop for the program.
    HideQueuedContinue,
    /// SIGCONT is reported sent by no process, so a session cannot tell the
    /// one it sent to end a held child's stop from the program's own.
    MisattributeContinues,
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

/// One debugger session: a controller tracing as a process of its own, and
/// the client task that drives it.
struct Session {
    tracer: i32,
    /// The controller, until it exits.
    controller: Option<SimController>,
    /// The client task, until it finishes.
    client: Option<ClientTask>,
    /// What the client did, for the trace.
    notes: Rc<RefCell<Vec<String>>>,
    woken: Arc<Woken>,
    waker: Waker,
    /// Whether the controller was asked to shut down.
    shutting_down: bool,
}

impl Session {
    fn new(
        tracer: i32,
        controller: SimController,
        client: ClientTask,
        notes: Rc<RefCell<Vec<String>>>,
    ) -> Self {
        let woken = Arc::new(Woken(AtomicBool::new(true)));
        let waker = Waker::from(Arc::clone(&woken));
        Self {
            tracer,
            controller: Some(controller),
            client: Some(client),
            notes,
            woken,
            waker,
            shutting_down: false,
        }
    }
}

struct World<'a> {
    swarm: Swarm,
    program: &'a Program,
    variant: &'a Variant,
    run: &'a Run,
    /// The bytes `AT_RANDOM` names.
    random: [u8; 16],
    machine: Rc<Machine>,
    /// The sessions, the first the client's.
    sessions: Vec<Session>,
    shared: Shared,
    auditor: Auditor,
    /// The children the first session held that no session took yet.
    waiting: VecDeque<HeldChild>,
    /// The children held that no session has seized, and that were not
    /// released, which the holding oracle watches.
    held: BTreeSet<Tid>,
    /// Each thread's latest arrival at a trap the controller heard of.
    arrivals: BTreeMap<Tid, Arrival>,
    /// The step the client requested, until it ends.
    stepping: Option<Begun>,
    /// The watchpoints the client knows of, with the bytes each watched at
    /// the last stop.
    watches: Vec<Intent>,
    baselines: BTreeMap<u64, Vec<u8>>,
    /// The hits each watchpoint had counted at the last stop judged.
    watch_counts: BTreeMap<u64, u64>,
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
            kernel.borrow_mut().sabotage = settings.sabotage;
        }
        let marks = Rc::new(RefCell::new(Marks::default()));
        let shared = Shared::default();
        let attach = swarm
            .attach
            .map(|_| start_untraced(&mut kernel.borrow_mut(), variant, run, random));
        let machine = Rc::new(Machine {
            kernel: Rc::clone(&kernel),
            waiters: RefCell::new(Vec::new()),
            choices: Rc::clone(&choices),
            scheduler: RefCell::new(scheduler),
            faults: RefCell::new(Faults::new(swarm.fault)),
            marks: Rc::clone(&marks),
            killed: Rc::clone(&shared.killed),
            ending: Rc::clone(&shared.ending),
            following: Rc::clone(&shared.following),
            unclean: RefCell::new(Vec::new()),
            leaked: RefCell::new(Vec::new()),
            step: Cell::new(0),
            ran: Cell::new(0),
            preempt: swarm.preempt,
            #[cfg(test)]
            sabotage: settings.sabotage,
        });
        let (controller, handle) =
            start_controller(&machine, variant, &swarm, random, TRACER, None);
        let auditor = Auditor::new(handle.subscribe(), Rc::clone(&shared.published));
        let client = Client {
            handle,
            choices,
            marks,
            shared: shared.clone(),
            script: script(&swarm, program, variant, run, attach),
            alone: Cell::new(None),
            baseline: RefCell::new(None),
            disabled_watches: RefCell::new(BTreeMap::new()),
            released_while_running: Cell::new(false),
        };
        let first = Session::new(
            TRACER,
            controller,
            Box::pin(client.run()),
            Rc::clone(&shared.notes),
        );
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
            random,
            machine,
            sessions: vec![first],
            shared,
            auditor,
            waiting: VecDeque::new(),
            held: BTreeSet::new(),
            arrivals: BTreeMap::new(),
            stepping: None,
            watches: Vec::new(),
            baselines: BTreeMap::new(),
            watch_counts: BTreeMap::new(),
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
        for (id, session) in self.sessions.iter().enumerate() {
            if let Some(controller) = &session.controller {
                if self.machine.can_collect(id) {
                    actions.push(Action::Collect(id));
                }
                if controller.has_message() {
                    actions.push(Action::Deliver(id));
                }
            }
            if session.client.is_some()
                && session.woken.0.load(Ordering::Relaxed)
                && (id != FIRST || self.client_may_act())
            {
                actions.push(Action::Poll(id));
            }
        }
        if !self.waiting.is_empty() {
            actions.push(Action::Follow);
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
        self.receive_held();
        self.flush();
        performed?;
        if action == Action::Poll(FIRST) {
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
                .controller()
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
            Action::Collect(id) => Ok(self.machine.collect(id)),
            Action::Deliver(FIRST) => self.deliver(),
            Action::Deliver(id) => self.deliver_to(id),
            Action::Poll(id) => {
                let session = &mut self.sessions[id];
                session.woken.0.store(false, Ordering::Relaxed);
                let task = session.client.as_mut().expect("poll needs a client");
                let polled = if id == FIRST {
                    "poll client".to_owned()
                } else {
                    format!("poll session {id}")
                };
                match task.as_mut().poll(&mut Context::from_waker(&session.waker)) {
                    Poll::Pending => Ok(polled),
                    Poll::Ready(result) => {
                        session.client = None;
                        self.flush();
                        result.map(|()| format!("{polled} -> done"))
                    }
                }
            }
            Action::Follow => self.follow(),
        }
    }

    /// The first session's controller, until it exits.
    fn controller(&self) -> Option<&SimController> {
        self.sessions[FIRST].controller.as_ref()
    }

    /// Takes the children the first session held since the last look,
    /// which the holding oracle watches from now on.
    fn receive_held(&mut self) {
        let mut receiver = self.shared.held.borrow_mut();
        let Some(receiver) = receiver.as_mut() else {
            return;
        };
        let kernel = self.machine.kernel.borrow();
        let mut marks = self.machine.marks.borrow_mut();
        while let Ok(child) = receiver.try_recv() {
            let tgid = tid_of(child.process().process_id);
            marks.hit(Mark::ChildHeld);
            if kernel
                .processes
                .get(&tgid)
                .is_some_and(|process| process.parent == Parent::Init)
            {
                marks.hit(Mark::HeldAfterParentExit);
            }
            self.trace.line(format!(
                "    {} held {tgid} for another session",
                child.parent()
            ));
            self.held.insert(tgid);
            self.waiting.push_back(child);
        }
    }

    /// Hands the next child held to a session of its own, as a client
    /// answering `startDebugging` does, or releases it, as one refusing
    /// does. Now and then the first session stops taking children, as one
    /// ending does, and its debugger releases those it forks from then on.
    fn follow(&mut self) -> Result<String, Failure> {
        let held = self
            .waiting
            .pop_front()
            .expect("follow needs a held child")
            .hand_over();
        let tgid = tid_of(held.process_id);
        let decision = self.machine.choices.borrow_mut().below(Stream::Client, 8);
        if decision == 1
            && let Some(receiver) = self.shared.held.borrow_mut().as_mut()
        {
            receiver.close();
            self.trace
                .line("    the first session takes no more children".to_owned());
        }
        if decision == 0 {
            self.release(held)?;
            return Ok(format!("follow {tgid} -> released"));
        }
        let id = self.sessions.len();
        let tracer = TRACER + i32::try_from(id).expect("few sessions");
        let (controller, handle) = start_controller(
            &self.machine,
            self.variant,
            &self.swarm,
            self.random,
            tracer,
            Some(held.start_time),
        );
        let notes = Rc::default();
        let requests = self.machine.choices.borrow_mut().below(Stream::Client, 8) + 1;
        let adopter = Adopter {
            handle,
            choices: Rc::clone(&self.machine.choices),
            marks: Rc::clone(&self.machine.marks),
            notes: Rc::clone(&notes),
            child: held,
            requests,
        };
        self.sessions.push(Session::new(
            tracer,
            controller,
            Box::pin(adopter.run()),
            notes,
        ));
        Ok(format!(
            "follow {tgid} -> session {id}, tracing as {tracer}"
        ))
    }

    /// Lets a held child run on, as `release_held` does for a client that
    /// will not adopt it: `SIGCONT` from the first session's process, once
    /// the child is known to be the one held.
    fn release(&mut self, held: HeldProcess) -> Result<(), Failure> {
        let tgid = tid_of(held.process_id);
        let mut kernel = self.machine.kernel.borrow_mut();
        let start_time = kernel
            .processes
            .get(&tgid)
            .map(|process| process.start_time);
        if start_time != Some(held.start_time) {
            return Err(Failure::debugger(
                "holding",
                format!(
                    "held child {tgid} started at {}, but its process started at {start_time:?}",
                    held.start_time
                ),
            ));
        }
        kernel.serve(TRACER);
        kernel
            .kill(tgid, libc::SIGCONT)
            .expect("a held child can be continued");
        self.held.remove(&tgid);
        self.machine.marks.borrow_mut().hit(Mark::HeldChildReleased);
        Ok(())
    }

    /// Lets a session adopting a held child handle the message at the
    /// front of its controller's queue.
    fn deliver_to(&mut self, id: SessionId) -> Result<String, Failure> {
        let session = &mut self.sessions[id];
        let controller = session
            .controller
            .as_mut()
            .expect("deliver needs a controller");
        let delivery = controller.take().expect("deliver needs a message");
        let description = format!("deliver to session {id}: {}", delivery.description);
        session.shutting_down |= delivery.shutdown;
        if controller.handle(delivery) {
            return Ok(description);
        }
        self.controller_exited(id)?;
        Ok(format!("{description} -> controller exited"))
    }

    /// A session's controller exited, and its tracer thread with it, which
    /// releases what it still traces (K-WAIT-3). It must hold no thread in
    /// a stop.
    fn controller_exited(&mut self, id: SessionId) -> Result<(), Failure> {
        let session = &mut self.sessions[id];
        session.controller = None;
        let tracer = session.tracer;
        let mut kernel = self.machine.kernel.borrow_mut();
        if let Some(thread) = kernel
            .threads
            .values()
            .find(|thread| thread.tracer() == Some(tracer) && thread.held())
        {
            return Err(Failure::debugger(
                "clean exit",
                format!(
                    "the controller exited holding thread {} in a stop",
                    thread.tid
                ),
            ));
        }
        kernel.forget_tracer(tracer);
        Ok(())
    }

    /// Lets the controller handle the message at the front of its queue,
    /// and checks the breakpoint hits it counted.
    fn deliver(&mut self) -> Result<String, Failure> {
        let session = &mut self.sessions[FIRST];
        let controller = session
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
            in_shutdown: session.shutting_down,
        });
        session.shutting_down |= delivery.shutdown;
        let running = controller.handle(delivery);
        if !running {
            self.controller_exited(FIRST)?;
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
        for arrival in self.arrivals.values_mut() {
            oracles::still_counting(&mut arrival.counted, &after, arrival.address);
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
                    targets,
                } => self.begin_step(thread, kind, presentation.as_ref(), &targets),
                Observation::StepEnded(reason) => self.judge_step(reason.as_ref())?,
                Observation::Variables {
                    stop,
                    variables,
                    backtrace,
                    evaluations,
                } if self.still_at(stop) => {
                    let source = self
                        .program
                        .source
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or_default();
                    let mut reached = semantics::evaluations(
                        &self.machine.kernel.borrow(),
                        &variables,
                        &backtrace,
                        self.variant,
                        source,
                        &self.program.markers,
                        &evaluations,
                    )
                    .map_err(|message| Failure::debugger("expressions", message))?;
                    reached.extend(
                        semantics::entry_values(
                            &self.machine.kernel.borrow(),
                            &variables,
                            &backtrace,
                            self.variant,
                        )
                        .map_err(|message| Failure::debugger("entry values", message))?,
                    );
                    let mut marks = self.machine.marks.borrow_mut();
                    for mark in reached {
                        marks.hit(mark);
                    }
                    drop(marks);
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
                Observation::Presented {
                    stop,
                    name,
                    value,
                    whole,
                    paged,
                } if self.still_at(stop) => {
                    let reached = self
                        .judge_presented(&name, &value, &whole, &paged)
                        .map_err(|message| {
                            Failure::debugger("views", format!("`{name}`: {message}"))
                        })?;
                    let mut marks = self.machine.marks.borrow_mut();
                    for mark in reached {
                        marks.hit(mark);
                    }
                }
                Observation::Backtrace { .. }
                | Observation::Variables { .. }
                | Observation::Presented { .. } => {}
            }
        }
        Ok(())
    }

    /// Judges a container's presentation, and its pages, by the views
    /// oracle, against the memory of the global the program holds it in.
    fn judge_presented(
        &self,
        name: &str,
        value: &crate::InspectedValue,
        whole: &Result<Vec<crate::ValueChildPage>, String>,
        paged: &Result<Vec<crate::ValueChildPage>, String>,
    ) -> Result<Vec<Mark>, String> {
        let Some((_, image_address, _)) = self
            .variant
            .globals
            .iter()
            .find(|(global, ..)| global == name)
        else {
            return Ok(Vec::new());
        };
        let address = image_address + self.variant.image.bias();
        match &value.state {
            crate::VariableState::Available {
                source: crate::VariableValueSource::Memory(source),
                ..
            } if source.get() == address => {}
            state => {
                return Err(format!(
                    "the program holds it at {address:#x}, but the debugger read {state:?}"
                ));
            }
        }
        let kernel = self.machine.kernel.borrow();
        let Some(tgid) = self
            .controller()
            .and_then(|controller| controller.truth().inferior)
        else {
            return Ok(Vec::new());
        };
        let Some(process) = kernel.processes.get(&tgid) else {
            return Ok(Vec::new());
        };
        let memory = |at: u64, size: u64| process.space.read_user(at, size);
        let Some(truth) = views::truth(name, address, &memory) else {
            return Ok(Vec::new());
        };
        let items = |fetched: &Result<Vec<crate::ValueChildPage>, String>| match fetched {
            Ok(fetched) => {
                for one in fetched {
                    views::within(one, crate::InspectionLimits::default())?;
                }
                views::items(fetched)
            }
            Err(error) => Err(format!("reading its elements failed: {error}")),
        };
        let shown = views::shown(value, items(whole)?, items(paged)?)
            .ok_or_else(|| format!("its view presented nothing: {:?}", value.state))?;
        views::judge(&truth, &shown)
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
            .controller()
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
                every: watch.reports_every_access(),
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
            let reached = watches::judge(&watches::Stop {
                kernel: &kernel,
                tgid,
                reasons: &truth.reasons,
                intents: &self.watches,
                baselines: &self.baselines,
                last_counts: &self.watch_counts,
                counts: &truth.watchpoints,
            })
            .map_err(|message| Failure::debugger("watch accounting", message))?;
            let mut marks = self.machine.marks.borrow_mut();
            for mark in reached {
                marks.hit(mark);
            }
        }
        self.watch_counts.clone_from(&truth.watchpoints);
        // What a watch was asked before this stop no longer applies.
        for intent in self.shared.watches.borrow_mut().values_mut() {
            intent
                .policies
                .drain(..intent.policies.len().saturating_sub(1));
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
        let Some(truth) = self.controller().map(SimController::truth) else {
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
        targets: &BTreeSet<u64>,
    ) {
        let mut kernel = self.machine.kernel.borrow_mut();
        let tid = Tid::try_from(thread.get()).expect("a simulated tid fits");
        let Some(stepped) = kernel.threads.get(&tid) else {
            return;
        };
        let mut begun = Begun::new(stepped, kind, presentation);
        let bias = self.variant.image.bias();
        begun.targets = targets.iter().map(|address| address + bias).collect();
        kernel.tracking = Some(Tracking {
            tid,
            // An advance's location may be in a callee, which it must not
            // pass.
            depth: if kind == crate::StepKind::Advance {
                usize::MAX
            } else {
                begun.shadow.depth()
            },
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
            .controller()
            .and_then(|controller| controller.truth().public_stop);
        if begun.kind == crate::StepKind::Advance {
            let stopped = stop.is_some_and(|stop| self.still_at(crate::StopId::new(stop)));
            let judged = semantics::advance(
                &self.machine.kernel.borrow(),
                &begun,
                &tracking.positions,
                reason,
                stopped,
                self.variant,
            )
            .map_err(|message| Failure::debugger("stepping", message))?;
            if judged.is_some() {
                self.machine.marks.borrow_mut().hit(Mark::AdvanceJudged);
            }
            return Ok(());
        }
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
        for (id, session) in self.sessions.iter().enumerate() {
            for note in session.notes.borrow_mut().drain(..) {
                if id == FIRST {
                    self.trace.line(format!("    client: {note}"));
                } else {
                    self.trace.line(format!("    session {id}: {note}"));
                }
            }
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
        if let Some(tid) = self.machine.leaked.borrow().first() {
            return Err(Failure::debugger(
                "transparency",
                format!("thread {tid} received SIGCONT, which only a debugger sends"),
            ));
        }
        self.check_held()?;
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
        let truth = self.controller().map(SimController::truth);
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
        for (id, session) in self.sessions.iter().enumerate() {
            let Some(controller) = &session.controller else {
                continue;
            };
            let truth = controller.truth();
            oracles::code_integrity(&kernel, &truth, &self.variant.image)
                .map_err(|message| Failure::debugger("code integrity", message))?;
            oracles::site_ownership(&kernel, &truth)
                .map_err(|message| Failure::debugger("site ownership", message))?;
            oracles::all_stop(&kernel, &truth)
                .map_err(|message| Failure::debugger("all-stop", message))?;
            if id == FIRST {
                oracles::user_breakpoints(
                    &kernel,
                    &truth,
                    &self.shared.intent(self.variant.image.bias()),
                )
                .map_err(|message| Failure::debugger("breakpoint accounting", message))?;
                oracles::disabled_breakpoints(&kernel, &truth, &self.shared.disabled.borrow())
                    .map_err(|message| Failure::debugger("breakpoint accounting", message))?;
            }
        }
        oracles::output_so_far(&kernel, self.run)
            .map_err(|message| Failure::debugger("transparency", message))?;
        Ok(())
    }

    /// Holding: every child held is as the oracle requires until a session
    /// seizes it.
    fn check_held(&mut self) -> Result<(), Failure> {
        let kernel = self.machine.kernel.borrow();
        let mut seized = Vec::new();
        for &tgid in &self.held {
            if !oracles::held(&kernel, tgid)
                .map_err(|message| Failure::debugger("holding", message))?
            {
                seized.push(tgid);
            }
        }
        for tgid in seized {
            self.held.remove(&tgid);
        }
        Ok(())
    }

    /// Ends a run in which nothing more can happen.
    fn finish(&self) -> Result<Progress, Failure> {
        for (id, session) in self.sessions.iter().enumerate() {
            if session.client.is_some() {
                return Err(Failure::debugger(
                    "liveness",
                    format!(
                        "the client of session {id} waits but nothing can happen: {}",
                        self.dump()
                    ),
                ));
            }
            if session.controller.is_some() {
                return Err(Failure::debugger(
                    "liveness",
                    format!(
                        "the controller of session {id} kept running after it answered the \
                         shutdown"
                    ),
                ));
            }
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
        for (id, session) in self.sessions.iter().enumerate() {
            let waiter = Rc::clone(&self.machine.waiters.borrow()[id]);
            let waiter = waiter.borrow();
            let _ = write!(
                text,
                "\n  session {id}, tracing as {}: waiter {}, holding {:?}; controller {}; \
                 client {}",
                session.tracer,
                if waiter.started() {
                    "started"
                } else {
                    "not started"
                },
                waiter.holding(),
                match &session.controller {
                    Some(controller) if controller.has_message() => {
                        "running, with messages queued"
                    }
                    Some(_) => "running, queue empty",
                    None => "exited",
                },
                if session.client.is_some() {
                    "waiting"
                } else {
                    "done"
                },
            );
        }
        text
    }
}

/// Starts a controller tracing as `tracer`, with a waiter of its own, which
/// checks the start time of a process it attaches to against `start_time`.
/// Returns it with the handle a client drives it through.
fn start_controller(
    machine: &Rc<Machine>,
    variant: &Variant,
    swarm: &Swarm,
    random: [u8; 16],
    tracer: i32,
    start_time: Option<u64>,
) -> (SimController, DebuggerHandle) {
    let waiter = Rc::new(RefCell::new(SimWaiter::new(tracer)));
    machine.waiters.borrow_mut().push(Rc::clone(&waiter));
    let debug_info = variant.debug_info();
    let module_image = Arc::clone(&debug_info.image);
    let (controller, channels) = SimController::new(SimParts {
        tracer,
        kernel: Rc::clone(&machine.kernel),
        waiter,
        preemption: Rc::clone(machine) as Rc<dyn Preemption>,
        launch: SimLaunch {
            image: Arc::clone(&variant.image),
            path: Arc::clone(&variant.path),
            random,
        },
        executable: SimExecutable {
            path: Arc::clone(&variant.path),
            data: Arc::clone(&variant.data),
            inode: variant.inode,
            start_time,
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
    (controller, handle)
}

/// The simulated thread a process identifier names.
fn tid_of(process: ProcessId) -> Tid {
    Tid::try_from(process.get()).expect("a simulated process id fits")
}

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
        markers: program
            .markers
            .iter()
            .map(|marker| (marker.line, marker.clone()))
            .collect(),
        marker_rows: marker_rows(program, variant),
        globals: variant.globals.clone(),
        views: program.views.clone(),
        debug: swarm.debug,
        image: Arc::clone(&variant.image),
        attach,
        follow: swarm.follow,
    }
}

/// Where, in unoptimized code, a row of a marker's line starts, with the
/// line, by image address.
fn marker_rows(program: &Program, variant: &Variant) -> BTreeMap<u64, u64> {
    let facts = &variant.facts;
    let source = program
        .source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if facts.optimized {
        return BTreeMap::new();
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
/// completed, and the breakpoints that counted it and still own the site.
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
