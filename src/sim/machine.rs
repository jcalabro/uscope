//! The simulated machine the controller's calls reach: the kernel, the
//! waiter, and the faults that may strike between any two calls.
//!
//! The world performs its actions through the machine, and the controller's
//! edge holds it for preemption: before each call into the kernel takes
//! effect, threads may run and the waiter may reap, as on Linux.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::rc::Rc;

use nix::libc;

use super::choices::{Choices, Stream};
use super::faults::Faults;
use super::kernel::{Happening, Kernel, State, Tid};
use super::marks::{Mark, Marks};
use super::schedule::{Action, Scheduler};
#[cfg(test)]
use super::world::Sabotage;
use crate::backend::sim_edge::{Preemption, SimWaiter};

/// The most instructions a thread runs at one preemption point.
const PREEMPT_BURST: u64 = 8;

/// The simulated machine the controller's calls reach: the kernel, the
/// waiter, and the faults that may strike between any two calls. The
/// controller's edge holds it for preemption.
pub struct Machine {
    pub kernel: Rc<RefCell<Kernel>>,
    pub waiter: Rc<RefCell<SimWaiter>>,
    pub choices: Rc<RefCell<Choices>>,
    pub scheduler: RefCell<Scheduler>,
    pub faults: RefCell<Faults>,
    pub marks: Rc<RefCell<Marks>>,
    /// Processes something outside the session killed.
    pub killed: Rc<RefCell<BTreeSet<Tid>>>,
    /// Processes that began to end as a whole.
    pub ending: Rc<RefCell<BTreeSet<Tid>>>,
    /// The action in progress.
    pub step: Cell<u64>,
    /// How often, in a thousand, a call is preempted.
    pub preempt: u64,
    #[cfg(test)]
    pub sabotage: Option<Sabotage>,
}

impl Machine {
    /// Counts what the kernel did since the last look, and fires a fault
    /// planned for a new thread. Returns lines for the trace.
    pub fn absorb(&self) -> Vec<String> {
        // The client may see any request about a process ending as a whole
        // fail.
        self.ending.borrow_mut().extend(
            self.kernel
                .borrow()
                .processes
                .values()
                .filter(|process| process.group_exit.is_some())
                .map(|process| process.tgid),
        );
        let happenings = std::mem::take(&mut self.kernel.borrow_mut().happenings);
        let mut lines = Vec::new();
        for happening in happenings {
            match happening {
                Happening::Cloned { parent, child } => {
                    self.marks.borrow_mut().hit(Mark::ThreadCreated);
                    lines.push(format!("{parent} created {child}"));
                    if self.faults.borrow_mut().cloned() {
                        lines.extend(self.kill(Mark::KilledNearClone));
                    }
                }
                Happening::GroupExit { tid } => lines.push(format!("{tid} exits its group")),
                Happening::PulledFromStop { tid } => {
                    let externally = self
                        .kernel
                        .borrow()
                        .process_of(tid)
                        .is_some_and(|process| process.killed_externally);
                    if !externally {
                        self.marks.borrow_mut().hit(Mark::GroupExitEndedStop);
                    }
                    lines.push(format!("{tid} was taken out of its stop"));
                }
                Happening::LeaderExitedAlone { tid } => {
                    self.marks.borrow_mut().hit(Mark::LeaderExitedAlone);
                    lines.push(format!("leader {tid} exited alone"));
                }
                Happening::Yielded { tid } => self.scheduler.borrow_mut().yielded(tid),
            }
        }
        lines
    }

    /// Kills the running program from outside, as the planned fault.
    /// Returns a line for the trace, or nothing when no program runs.
    pub fn kill(&self, mark: Mark) -> Option<String> {
        let mut kernel = self.kernel.borrow_mut();
        let tgid = *kernel
            .processes
            .iter()
            .find(|(_, process)| process.group_exit.is_none())?
            .0;
        kernel.processes.get_mut(&tgid)?.killed_externally = true;
        kernel
            .kill(tgid, libc::SIGKILL)
            .expect("a live process can be killed");
        self.killed.borrow_mut().insert(tgid);
        self.ending.borrow_mut().insert(tgid);
        self.faults.borrow_mut().fired = Some(self.step.get());
        self.marks.borrow_mut().hit(mark);
        Some(format!("fault: SIGKILL from outside to {tgid}"))
    }

    /// Reaps one status, if the waiter can, choosing among the ready ones.
    pub fn collect(&self) -> String {
        let mut kernel = self.kernel.borrow_mut();
        let collected = self.waiter.borrow_mut().collect(&mut kernel, |ready| {
            *self.choices.borrow_mut().pick(Stream::Schedule, ready)
        });
        let mut line = String::from("collect");
        if let Some(status) = collected.reaped {
            let _ = write!(line, " {status}");
        }
        if collected.held {
            self.marks.borrow_mut().hit(Mark::QueueFull);
            line.push_str(" -> held, the queue is full");
        } else {
            line.push_str(" -> queued");
        }
        line
    }

    /// Whether the waiter can collect now.
    pub fn can_collect(&self) -> bool {
        #[cfg(test)]
        if self.sabotage == Some(Sabotage::DeafWaiter) {
            return false;
        }
        self.waiter.borrow().can_collect(&self.kernel.borrow())
    }

    /// Runs `tid` for up to `budget` instructions. Returns a line for the
    /// trace.
    pub fn run_thread(&self, tid: Tid, budget: u64) -> String {
        let mut kernel = self.kernel.borrow_mut();
        let ran = kernel.run(tid, budget);
        let state = kernel.threads.get(&tid).map(|thread| thread.state);
        format!(
            "run {tid} x{}{} -> {}",
            ran.executed,
            if ran.yielded { " yielding" } else { "" },
            describe_state(state)
        )
    }
}

impl Preemption for Machine {
    fn before_call(&self) {
        if self.faults.borrow_mut().call()
            && let Some(line) = self.kill(Mark::KilledInsideCall)
        {
            record_inside_call(&line);
        }
        if !self
            .choices
            .borrow_mut()
            .chance(Stream::Preempt, self.preempt)
        {
            return;
        }
        let actors = self.choices.borrow_mut().below(Stream::Preempt, 3) + 1;
        for _ in 0..actors {
            let mut options = self
                .kernel
                .borrow()
                .runnable()
                .map(Action::Run)
                .collect::<Vec<_>>();
            if self.can_collect() {
                options.push(Action::Collect);
            }
            if options.is_empty() {
                return;
            }
            let action = *self.choices.borrow_mut().pick(Stream::Preempt, &options);
            let line = if let Action::Run(tid) = action {
                let budget = self
                    .choices
                    .borrow_mut()
                    .below(Stream::Preempt, PREEMPT_BURST)
                    + 1;
                self.marks.borrow_mut().hit(Mark::PreemptedCall);
                self.run_thread(tid, budget)
            } else {
                self.marks.borrow_mut().hit(Mark::ReapedInsideCall);
                self.collect()
            };
            record_inside_call(&format!("preempt: {line}"));
            for line in self.absorb() {
                record_inside_call(&format!("preempt: {line}"));
            }
        }
    }
}

/// Records what happened inside one of the controller's calls among the
/// controller's own lines, in the order it happened. Only development builds
/// record.
#[cfg_attr(
    not(debug_assertions),
    expect(clippy::missing_const_for_fn, reason = "development builds record")
)]
fn record_inside_call(line: &str) {
    #[cfg(debug_assertions)]
    record!("{line}");
    #[cfg(not(debug_assertions))]
    let _ = line;
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
