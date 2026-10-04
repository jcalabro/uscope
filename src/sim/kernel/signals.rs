//! Signals: what they do by default, how they are delivered, and how
//! threads exit.

use nix::errno::Errno;
use nix::libc;

use super::{ExitStatus, Kernel, SigInfo, State, StopKind, Tid, WaitStatus};

pub const SIGTRAP: i32 = libc::SIGTRAP;
pub const SIGKILL: i32 = libc::SIGKILL;
pub const SIGSTOP: i32 = libc::SIGSTOP;

/// `si_code` of a signal sent by `kill`.
pub const SI_USER: i32 = libc::SI_USER;
/// `si_code` of a signal sent by `tgkill`.
pub const SI_TKILL: i32 = libc::SI_TKILL;
/// `si_code` of a breakpoint trap.
pub const TRAP_BRKPT: i32 = 1;
/// `si_code` of a single-step trap.
pub const TRAP_TRACE: i32 = 2;

/// Signals the kernel delivers before any other pending signal, because an
/// instruction raised them.
const SYNCHRONOUS: [i32; 6] = [
    libc::SIGSEGV,
    libc::SIGBUS,
    libc::SIGILL,
    SIGTRAP,
    libc::SIGFPE,
    libc::SIGSYS,
];

/// A set of standard signals, 1 to 31.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SignalSet(u32);

impl SignalSet {
    pub const fn insert(&mut self, signal: i32) {
        self.0 |= 1 << (signal - 1);
    }

    pub const fn remove(&mut self, signal: i32) {
        self.0 &= !(1 << (signal - 1));
    }

    #[must_use]
    pub const fn contains(self, signal: i32) -> bool {
        self.0 & (1 << (signal - 1)) != 0
    }

    /// The signal the kernel delivers next: synchronous signals first, then
    /// the lowest-numbered.
    fn next(self) -> Option<i32> {
        SYNCHRONOUS
            .into_iter()
            .find(|&signal| self.contains(signal))
            .or_else(|| (1..32).find(|&signal| self.contains(signal)))
    }
}

/// What a signal does when the program has no handler for it.
enum DefaultAction {
    Terminate,
    CoreDump,
    Ignore,
    Stop,
}

const fn default_action(signal: i32) -> DefaultAction {
    match signal {
        libc::SIGQUIT
        | libc::SIGILL
        | libc::SIGTRAP
        | libc::SIGABRT
        | libc::SIGBUS
        | libc::SIGFPE
        | libc::SIGSEGV
        | libc::SIGXCPU
        | libc::SIGXFSZ
        | libc::SIGSYS => DefaultAction::CoreDump,
        libc::SIGCHLD | libc::SIGCONT | libc::SIGURG | libc::SIGWINCH => DefaultAction::Ignore,
        libc::SIGSTOP | libc::SIGTSTP | libc::SIGTTIN | libc::SIGTTOU => DefaultAction::Stop,
        _ => DefaultAction::Terminate,
    }
}

/// A signal's conventional name.
#[must_use]
pub fn name(signal: i32) -> String {
    nix::sys::signal::Signal::try_from(signal).map_or_else(
        |_| format!("signal {signal}"),
        |signal| signal.as_str().to_owned(),
    )
}

impl Kernel {
    /// `kill(tgid, signal)`, sent by the tracer.
    pub fn kill(&mut self, group: Tid, signal: i32) -> Result<(), Errno> {
        if !self.processes.contains_key(&group) {
            return Err(Errno::ESRCH);
        }
        if signal != SIGKILL {
            self.gap(format!("kill with {}", name(signal)));
            return Err(Errno::ENOSYS);
        }
        self.kill_process(group, ExitStatus::Signal(SIGKILL, false));
        Ok(())
    }

    /// `tgkill(tgid, tid, signal)`, sent by the tracer. K-INT-2: a thread
    /// that has not been reaped, even a zombie, accepts it.
    pub fn tgkill(&mut self, group: Tid, tid: Tid, signal: i32) -> Result<(), Errno> {
        let Some(thread) = self
            .threads
            .get_mut(&tid)
            .filter(|thread| thread.tgid == group)
        else {
            return Err(Errno::ESRCH);
        };
        if signal != SIGSTOP {
            self.gap(format!("tgkill with {}", name(signal)));
            return Err(Errno::ENOSYS);
        }
        // K-SIG-1: delivered at the thread's next chance as a stop from
        // the tracer.
        thread.pending.insert(signal);
        Ok(())
    }

    /// Ends every thread of a process with `exit` (K-EXIT-3). A thread
    /// already at its exit event stays there (K-EXIT-4); any other is taken
    /// out of its stop, losing a status not yet reported (K-WAIT-1), and
    /// runs to its exit.
    pub(super) fn kill_process(&mut self, group: Tid, exit: ExitStatus) {
        if let Some(process) = self.processes.get_mut(&group) {
            process.exit.get_or_insert(exit);
        }
        for thread in self
            .threads
            .values_mut()
            .filter(|thread| thread.tgid == group)
        {
            match thread.state {
                State::Running
                | State::Stopped {
                    kind: StopKind::Signal(_),
                    ..
                } => {
                    thread.state = State::Exiting(exit);
                    thread.report = None;
                }
                State::Stopped { .. } | State::Exiting(_) | State::Zombie(_) => {}
            }
        }
    }

    /// Delivers a running thread's next pending signal, which stops it for
    /// the tracer. Returns whether it stopped.
    pub(super) fn deliver_pending(&mut self, tid: Tid) -> bool {
        let tracer = self.tracer;
        let thread = self.threads.get_mut(&tid).expect("running thread exists");
        let Some(signal) = thread.pending.next() else {
            return false;
        };
        thread.pending.remove(signal);
        if signal != SIGSTOP {
            self.gap(format!("delivering pending {}", name(signal)));
            return true;
        }
        self.signal_stop(
            tid,
            SigInfo {
                signal,
                code: SI_TKILL,
                pid: tracer,
                address: 0,
            },
        );
        true
    }

    /// Resumes a thread from a signal-delivery-stop with `signal` delivered
    /// to it, or suppressed when `None`.
    pub(super) fn resume_with(&mut self, tid: Tid, signal: Option<i32>) {
        let thread = self.threads.get_mut(&tid).expect("resumed thread exists");
        thread.state = State::Running;
        thread.report = None;
        let Some(signal) = signal else {
            return;
        };
        match default_action(signal) {
            DefaultAction::Ignore => {}
            DefaultAction::Terminate => {
                let group = thread.tgid;
                self.kill_process(group, ExitStatus::Signal(signal, false));
            }
            DefaultAction::CoreDump => {
                let group = thread.tgid;
                // Core dumps are off for simulated processes, as with a
                // zero core size limit.
                self.kill_process(group, ExitStatus::Signal(signal, false));
            }
            DefaultAction::Stop => self.gap(format!("group-stop by {}", name(signal))),
        }
    }

    /// Takes an exiting thread to its exit: the exit event stop when exits
    /// are traced (K-EXIT-1, K-EXIT-3), otherwise straight to a zombie.
    pub(super) fn reach_exit(&mut self, tid: Tid, exit: ExitStatus) {
        let thread = self.threads.get_mut(&tid).expect("exiting thread exists");
        if thread.options.trace_exit {
            thread.state = State::Stopped {
                kind: StopKind::Exit(exit),
                info: SigInfo {
                    signal: SIGTRAP,
                    code: SIGTRAP | (libc::PTRACE_EVENT_EXIT << 8),
                    ..SigInfo::default()
                },
            };
            thread.report = Some(WaitStatus::Event(tid, libc::PTRACE_EVENT_EXIT));
        } else {
            self.become_zombie(tid, exit);
        }
    }

    /// Ends a thread for good; its status waits to be reaped.
    pub(super) fn become_zombie(&mut self, tid: Tid, exit: ExitStatus) {
        let thread = self.threads.get_mut(&tid).expect("exiting thread exists");
        let group = thread.tgid;
        thread.state = State::Zombie(exit);
        thread.report = Some(exit.wait_status(tid));
        if let Some(process) = self.processes.get_mut(&group) {
            process.exit.get_or_insert(exit);
        }
    }
}
