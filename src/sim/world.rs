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
use std::collections::BTreeSet;
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
use super::client::{Client, Script, Shared};
use super::corpus::{Corpus, Run, Variant};
use super::faults::{Faults, Plan};
use super::kernel::{Kernel, State, StopKind, Tid};
use super::machine::Machine;
use super::marks::{Mark, Marks};
use super::oracles;
use super::report::{Failure, Trace};
use super::schedule::{Action, Scheduler};
use super::swarm::Swarm;
use crate::DebuggerHandle;
use crate::backend::sim_edge::{
    Preemption, SimController, SimExecutable, SimLaunch, SimParts, SimWaiter,
};

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
    /// The traps whose stops the controller heard of, by thread and count.
    counted: BTreeSet<(Tid, u64)>,
    trace: Trace,
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
        let scheduler = Scheduler::new(swarm.policy, &mut choices);
        let choices = Rc::new(RefCell::new(choices));
        let kernel = Rc::new(RefCell::new(Kernel::new(TRACER)));
        #[cfg(test)]
        {
            let mut kernel = kernel.borrow_mut();
            kernel.lose_pokes = settings.sabotage == Some(Sabotage::LosePokes);
            kernel.skip_traps = settings.sabotage == Some(Sabotage::SkipTraps);
        }
        let marks = Rc::new(RefCell::new(Marks::default()));
        let shared = Shared::default();
        let machine = Rc::new(Machine {
            kernel: Rc::clone(&kernel),
            waiter: Rc::new(RefCell::new(SimWaiter::default())),
            choices: Rc::clone(&choices),
            scheduler: RefCell::new(scheduler),
            faults: RefCell::new(Faults::new(swarm.fault)),
            marks: Rc::clone(&marks),
            killed: Rc::clone(&shared.killed),
            ending: Rc::clone(&shared.ending),
            step: Cell::new(0),
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
        let auditor = Auditor::new(handle.subscribe());
        let client = Client {
            handle,
            choices,
            marks,
            shared: shared.clone(),
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
            alone: Cell::new(None),
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
            machine,
            controller: Some(controller),
            client: Some(Box::pin(client.run())),
            shared,
            woken,
            waker,
            auditor,
            counted: BTreeSet::new(),
            trace,
            #[cfg(debug_assertions)]
            capture,
        }
    }

    fn step(&self) -> u64 {
        self.machine.step.get()
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
        if self.client.is_some() && self.woken.0.load(Ordering::Relaxed) {
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
            self.machine.kernel.borrow_mut().user_breakpoints = self.shared.addresses();
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
        // A trap whose stop the controller now hears of, once: each trap the
        // CPU executed is one hit.
        let trap = delivery
            .trap
            .and_then(|tid| trap_address(&self.machine.kernel.borrow(), tid))
            .filter(|&(tid, _, traps)| self.counted.insert((tid, traps)))
            .map(|(tid, address, _)| (tid, address));
        let launch = delivery.launch;
        let running = controller.handle(delivery);
        if !running {
            self.controller = None;
            return Ok(format!("deliver {description} -> controller exited"));
        }
        let after = controller.truth();
        // A group exit or SIGKILL meanwhile takes the trap's thread out of
        // its stop, and the controller may never count the hit.
        let disturbed = trap.is_some_and(|(tid, _)| {
            self.machine
                .kernel
                .borrow()
                .process_of(tid)
                .is_none_or(|process| process.group_exit.is_some())
        });
        oracles::hit_counts(&before, &after, trap, launch, disturbed)
            .map_err(|message| Failure::debugger("breakpoint accounting", message))?;
        Ok(format!("deliver {description}"))
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
            oracles::user_breakpoints(&kernel, &truth, &self.shared.breakpoints.borrow())
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

/// The address of the trap whose stop `tid` is in, if its stop is one, and
/// how many traps the thread executed so far.
fn trap_address(kernel: &Kernel, tid: Tid) -> Option<(Tid, u64, u64)> {
    let thread = kernel.threads.get(&tid)?;
    match thread.state {
        State::Stopped {
            kind: StopKind::Signal(libc::SIGTRAP),
            info,
        } if info.code == super::cpu::SI_KERNEL => thread
            .trapped_at
            .map(|address| (tid, address, thread.traps)),
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
