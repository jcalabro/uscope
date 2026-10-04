//! The event auditor: reads every event the debugger publishes, as a client
//! that never falls behind would, and checks the stream's promises against
//! the kernel.

use tokio::sync::broadcast;

use super::kernel::{ExitStatus, Kernel, Tid, WaitStatus};
use super::marks::{Mark, Marks};
use super::report::Failure;
use crate::DebuggerEvent;

pub struct Auditor {
    events: broadcast::Receiver<DebuggerEvent>,
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
    pub const fn new(events: broadcast::Receiver<DebuggerEvent>) -> Self {
        Self {
            events,
            revision: 0,
            change: 0,
            stop: 0,
        }
    }

    /// Reads the events published since the last action: each state change
    /// announces a new revision, which the events it produced share, so
    /// revisions never decrease; stop identifiers only increase; and every
    /// exit the client hears of, of a thread or of the process, is the one
    /// the kernel reported.
    pub fn check(&mut self, kernel: &Kernel, marks: &mut Marks) -> Result<(), Failure> {
        loop {
            let event = match self.events.try_recv() {
                Ok(event) => event,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
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
                }
                DebuggerEvent::ThreadExited {
                    thread_id, status, ..
                } => {
                    marks.hit(Mark::ThreadExited);
                    let tid = Tid::try_from(thread_id.get()).expect("a simulated tid fits");
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
                    let Some((truth, _)) = kernel.ended.get(&tgid) else {
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
                _ => {}
            }
        }
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
        WaitStatus::Stopped(..) | WaitStatus::Event(..) => false,
    }
}
