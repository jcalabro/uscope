//! The event auditor: reads every event the debugger publishes, as a client
//! that never falls behind would, and checks the stream's promises against
//! the kernel.

use tokio::sync::broadcast;

use std::cell::RefCell;
use std::rc::Rc;

use super::hits::Published;
use super::kernel::{ExitStatus, Kernel, Tid, WaitStatus};
use super::marks::{Mark, Marks};
use super::report::Failure;
use crate::{ConditionOwner, DebuggerEvent};

pub struct Auditor {
    events: broadcast::Receiver<DebuggerEvent>,
    /// The hit events counted for the client's checks.
    published: Rc<RefCell<Published>>,
    /// The newest revision of any event.
    revision: u64,
    /// The newest revision a `StateChanged` announced.
    change: u64,
    /// The newest stop published.
    stop: u64,
}

fn failure(message: String) -> Failure {
    Failure::debugger("events", message)
}

impl Auditor {
    pub const fn new(
        events: broadcast::Receiver<DebuggerEvent>,
        published: Rc<RefCell<Published>>,
    ) -> Self {
        Self {
            events,
            published,
            revision: 0,
            change: 0,
            stop: 0,
        }
    }

    /// Reads the events published since the last action: each state change
    /// announces a new revision, which the events it produced share, so
    /// revisions never decrease; stop identifiers only increase; and every
    /// exit the client hears of, of a thread or of the process, is the one
    /// the kernel reported, or, for a leader, the code it passed to `exit`.
    /// It counts the messages hits logged and the conditions that failed to
    /// evaluate, for the client's checks.
    pub fn check(&mut self, kernel: &Kernel, marks: &mut Marks) -> Result<(), Failure> {
        loop {
            let event = match self.events.try_recv() {
                Ok(event) => event,
                Err(broadcast::error::TryRecvError::Lagged(_)) => {
                    self.published.borrow_mut().gaps += 1;
                    continue;
                }
                Err(_) => return Ok(()),
            };
            let revision = event.revision();
            if revision < self.revision {
                return Err(failure(format!(
                    "revision {revision} follows {} in {event:?}",
                    self.revision
                )));
            }
            self.revision = revision;
            match &event {
                DebuggerEvent::StateChanged { revision } => {
                    if *revision <= self.change {
                        return Err(failure(format!(
                            "state change {revision} follows state change {}",
                            self.change
                        )));
                    }
                    self.change = *revision;
                }
                DebuggerEvent::InferiorStopped { stop_id, .. } => {
                    if stop_id.get() <= self.stop {
                        return Err(failure(format!(
                            "stop {stop_id} follows stop {}",
                            self.stop
                        )));
                    }
                    self.stop = stop_id.get();
                    self.published.borrow_mut().stops.insert(stop_id.get());
                }
                DebuggerEvent::ThreadExited {
                    thread_id, status, ..
                } => {
                    marks.hit(Mark::ThreadExited);
                    let tid = Tid::try_from(thread_id.get()).expect("a simulated tid fits");
                    // A leader that exits while the debugger knows other
                    // threads live is reported at its exit event, with the
                    // code it passed to `exit`: Linux reports its exit only
                    // once every other thread has exited, with the process's
                    // status. Those threads may have begun to exit already.
                    if let Some(&code) = kernel.leader_exits.get(&tid) {
                        if !same_exit(ExitStatus::Code(code), status) {
                            return Err(failure(format!(
                                "leader {tid} passed {code} to exit, but the client heard \
                                 {status:?}"
                            )));
                        }
                        marks.hit(Mark::LeaderExitReported);
                        continue;
                    }
                    let Some(&reaped) = kernel.reaped.get(&tid) else {
                        return Err(failure(format!(
                            "thread {tid} reported {status:?} before it was reaped"
                        )));
                    };
                    if !same_status(reaped, status) {
                        return Err(failure(format!(
                            "thread {tid} reported {reaped}, but the client heard {status:?}"
                        )));
                    }
                }
                DebuggerEvent::InferiorExited {
                    process_id, status, ..
                } => {
                    let tgid = Tid::try_from(process_id.get()).expect("a simulated pid fits");
                    let Some(truth) = kernel.ended.get(&tgid).map(|ended| &ended.status) else {
                        return Err(failure(format!(
                            "process {tgid} reported {status:?} before it ended"
                        )));
                    };
                    if matches!(truth, ExitStatus::Code(_)) {
                        marks.hit(Mark::ProgramExited);
                    }
                    if !same_exit(*truth, status) {
                        return Err(failure(format!(
                            "process {tgid} ended {truth:?}, but the client heard {status:?}"
                        )));
                    }
                }
                DebuggerEvent::LogMessage { .. } | DebuggerEvent::ConditionFailed { .. } => {
                    self.count_hit_event(&event)?;
                }
                _ => {}
            }
        }
    }

    /// Counts a message a hit logged or a condition that failed to evaluate.
    fn count_hit_event(&self, event: &DebuggerEvent) -> Result<(), Failure> {
        let mut published = self.published.borrow_mut();
        match event {
            DebuggerEvent::LogMessage { breakpoint, .. } => {
                *published.logged.entry(breakpoint.get()).or_default() += 1;
            }
            DebuggerEvent::ConditionFailed {
                owner: ConditionOwner::Breakpoint(breakpoint),
                ..
            } => {
                *published
                    .condition_failures
                    .entry(breakpoint.get())
                    .or_default() += 1;
            }
            // The client's watch conditions are constants.
            DebuggerEvent::ConditionFailed {
                owner: ConditionOwner::Watchpoint(watchpoint),
                error,
                ..
            } => {
                return Err(failure(format!(
                    "watchpoint {watchpoint}'s constant condition failed to evaluate: {error}"
                )));
            }
            _ => {}
        }
        Ok(())
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

fn same_status(kernel: WaitStatus, reported: &crate::ExitStatus) -> bool {
    match kernel {
        WaitStatus::Exited(_, code) => same_exit(ExitStatus::Code(code), reported),
        WaitStatus::Signaled(_, signal, core) => {
            same_exit(ExitStatus::Signal(signal, core), reported)
        }
        WaitStatus::Stopped(..) | WaitStatus::Event(..) | WaitStatus::GroupStop(..) => false,
    }
}
