//! Signals: what they do by default, how they are delivered, and how
//! threads exit.

use nix::errno::Errno;
use nix::libc;

use super::{ExitStatus, Happening, Kernel, SigInfo, State, StopKind, Tid, WaitStatus};

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
/// `si_code` of a hardware breakpoint's trap.
pub const TRAP_HWBKPT: i32 = 4;

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

/// The standard signals, 1 to 31, pending for one thread, each with the
/// siginfo it will be delivered with. A standard signal already pending is
/// not queued again: the first one's siginfo stays.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pending([Option<SigInfo>; 31]);

impl Pending {
    /// Makes `info.signal` pending, unless it already is.
    pub const fn insert(&mut self, info: SigInfo) {
        let slot = &mut self.0[(info.signal - 1).cast_unsigned() as usize];
        if slot.is_none() {
            *slot = Some(info);
        }
    }

    #[must_use]
    pub const fn contains(&self, signal: i32) -> bool {
        self.0[(signal - 1).cast_unsigned() as usize].is_some()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        let mut index = 0;
        while index < self.0.len() {
            if self.0[index].is_some() {
                return false;
            }
            index += 1;
        }
        true
    }

    /// Takes the signal the kernel delivers next: synchronous signals
    /// first, then the lowest-numbered.
    fn take_next(&mut self) -> Option<SigInfo> {
        let signal = SYNCHRONOUS
            .into_iter()
            .find(|&signal| self.contains(signal))
            .or_else(|| (1..32).find(|&signal| self.contains(signal)))?;
        self.0[(signal - 1).cast_unsigned() as usize].take()
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
    /// `kill(tgid, SIGKILL)`, sent by the tracer.
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

    /// `tgkill(tgid, tid, SIGSTOP)`, sent by the tracer. K-INT-2: a thread
    /// that has not been reaped, even a zombie, accepts it.
    pub fn tgkill(&mut self, group: Tid, tid: Tid, signal: i32) -> Result<(), Errno> {
        let tracer = self.tracer;
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
        thread.pending.insert(SigInfo {
            signal,
            code: SI_TKILL,
            pid: tracer,
            address: 0,
        });
        Ok(())
    }

    /// Ends every thread of a process with `exit`, as `exit_group` or a
    /// fatal signal does (K-EXIT-1, K-EXIT-3). Any thread in a stop is taken
    /// out of it, losing a status not yet reported (K-WAIT-1, K-EXIT-2):
    /// one at the exit event of its own `exit` finishes exiting, and any
    /// other runs to its exit. A group already exiting keeps its status,
    /// and its threads at their exit events stay there (K-EXIT-4).
    pub(super) fn kill_process(&mut self, group: Tid, exit: ExitStatus) {
        let Some(process) = self.processes.get_mut(&group) else {
            return;
        };
        if process.group_exit.is_some() {
            return;
        }
        process.group_exit = Some(exit);
        let mut pulled = Vec::new();
        let mut finished = Vec::new();
        for thread in self
            .threads
            .values_mut()
            .filter(|thread| thread.tgid == group)
        {
            match thread.state {
                State::Running => thread.state = State::Exiting(exit),
                State::Stopped {
                    kind: StopKind::Signal(_) | StopKind::Event(..),
                    ..
                } => {
                    thread.state = State::Exiting(exit);
                    thread.report = None;
                    // A system call the thread stopped inside returns on
                    // the way out.
                    if let Some(result) = thread.returning.take() {
                        thread.registers.general[super::RAX] = result;
                    }
                    pulled.push(thread.tid);
                }
                State::Stopped {
                    kind: StopKind::Exit(own),
                    ..
                } => finished.push((thread.tid, own)),
                State::Exiting(_) | State::Zombie(_) => {}
            }
        }
        for &(tid, own) in &finished {
            self.become_zombie(tid, own);
        }
        self.happenings.extend(
            pulled
                .into_iter()
                .chain(finished.into_iter().map(|(tid, _)| tid))
                .map(|tid| Happening::PulledFromStop { tid }),
        );
    }

    /// Delivers a running thread's next pending signal, its own before its
    /// process's. A traced thread stops for the tracer; an untraced one
    /// takes the signal's default action. Returns whether it stopped.
    pub(super) fn deliver_pending(&mut self, tid: Tid) -> bool {
        let thread = self.threads.get_mut(&tid).expect("running thread exists");
        let group = thread.tgid;
        let Some(info) = thread.pending.take_next().or_else(|| {
            self.processes
                .get_mut(&group)
                .and_then(|process| process.shared.take_next())
        }) else {
            return false;
        };
        if info.signal != SIGSTOP && info.signal != libc::SIGCHLD {
            self.gap(format!("delivering pending {}", name(info.signal)));
            return true;
        }
        self.signal_stop(tid, info);
        !matches!(self.threads[&tid].state, State::Running)
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

    /// Takes an untraced thread through `info`'s default action, as if no
    /// debugger were there: SIGCHLD is ignored, and a trap or fault ends
    /// its process (K-FORK-2).
    pub(super) fn act_by_default(&mut self, tid: Tid, info: SigInfo) {
        match default_action(info.signal) {
            DefaultAction::Ignore => {}
            DefaultAction::Terminate | DefaultAction::CoreDump => {
                let group = self.threads[&tid].tgid;
                // Core dumps are off for simulated processes, as with a zero
                // core size limit.
                self.kill_process(group, ExitStatus::Signal(info.signal, false));
            }
            DefaultAction::Stop => self.gap(format!("group-stop by {}", name(info.signal))),
        }
    }

    /// Takes an exiting thread to its exit: the exit event stop when exits
    /// are traced (K-EXIT-1, K-EXIT-3), otherwise straight to a zombie.
    pub(super) fn reach_exit(&mut self, tid: Tid, exit: ExitStatus) {
        let thread = self.threads.get_mut(&tid).expect("exiting thread exists");
        if thread.traced() && thread.options.trace_exit {
            thread.enter_stop(
                StopKind::Exit(exit),
                SigInfo::event(tid, libc::PTRACE_EVENT_EXIT),
                WaitStatus::Event(tid, libc::PTRACE_EVENT_EXIT),
            );
        } else {
            self.become_zombie(tid, exit);
        }
    }

    /// Ends a thread for good; its status waits to be reaped. A leader
    /// leaving others behind stays a zombie until they are gone (K-EXIT-5).
    pub(super) fn become_zombie(&mut self, tid: Tid, exit: ExitStatus) {
        let thread = self.threads.get_mut(&tid).expect("exiting thread exists");
        let group = thread.tgid;
        let traced = thread.traced();
        thread.state = State::Zombie(exit);
        // The tracer reaps a traced thread; nobody waits for an untraced one
        // but its process's parent.
        thread.report = traced.then(|| exit.wait_status(tid));
        thread.pending = Pending::default();
        self.forget_children(tid);
        let Some(process) = self.processes.get(&group) else {
            return;
        };
        let alone = process.group_exit.is_none()
            && self.threads.values().any(|thread| {
                thread.tgid == group
                    && thread.tid != tid
                    && !matches!(thread.state, State::Zombie(_))
            });
        if tid == group && alone {
            self.happenings.push(Happening::LeaderExitedAlone { tid });
        }
        if !traced {
            self.reap_untraced(tid);
        }
    }
}
