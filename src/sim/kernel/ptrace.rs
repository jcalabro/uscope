//! The tracer's ptrace requests. Each answers as Linux does, in terms of
//! the K-* rules; above all K-WAIT-1, that a request to a thread not in a
//! ptrace-stop fails with `ESRCH`.

use nix::errno::Errno;
use nix::libc;

use super::{Kernel, Options, SigInfo, State, StopKind, Thread, Tid, Tracing};
use crate::sim::cpu::Registers;

/// A ptrace event's name, as `PTRACE_EVENT_*` without the prefix.
#[must_use]
pub fn event_name(event: i32) -> String {
    match event {
        libc::PTRACE_EVENT_FORK => "FORK".into(),
        libc::PTRACE_EVENT_VFORK => "VFORK".into(),
        libc::PTRACE_EVENT_CLONE => "CLONE".into(),
        libc::PTRACE_EVENT_EXEC => "EXEC".into(),
        libc::PTRACE_EVENT_EXIT => "EXIT".into(),
        libc::PTRACE_EVENT_STOP => "STOP".into(),
        other => format!("{other}"),
    }
}

impl Kernel {
    /// The thread a request addresses, which must be in a ptrace-stop.
    fn stopped(&self, tid: Tid) -> Result<&Thread, Errno> {
        self.threads
            .get(&tid)
            .filter(|thread| thread.is_stopped())
            .ok_or(Errno::ESRCH)
    }

    fn stopped_mut(&mut self, tid: Tid) -> Result<&mut Thread, Errno> {
        self.threads
            .get_mut(&tid)
            .filter(|thread| thread.is_stopped())
            .ok_or(Errno::ESRCH)
    }

    /// `PTRACE_GETREGS`.
    pub fn get_registers(&self, tid: Tid) -> Result<Registers, Errno> {
        Ok(self.stopped(tid)?.registers)
    }

    /// `PTRACE_SETREGS`.
    pub fn set_registers(&mut self, tid: Tid, registers: Registers) -> Result<(), Errno> {
        self.stopped_mut(tid)?.registers = registers;
        Ok(())
    }

    /// `PTRACE_PEEKUSER` of a debug register (K-DR-3, K-DR-4).
    pub fn peek_debug(&self, tid: Tid, index: usize) -> Result<u64, Errno> {
        let thread = self.stopped(tid)?;
        #[cfg(test)]
        if let Some(phantom) = self.phantom_debug.get(&tid) {
            return phantom.peek(index);
        }
        match self.debug_behavior {
            super::DebugBehavior::Discarding => Ok(0),
            _ => thread.debug.peek(index),
        }
    }

    /// `PTRACE_POKEUSER` of a debug register (K-DR-4).
    pub fn poke_debug(&mut self, tid: Tid, index: usize, value: u64) -> Result<(), Errno> {
        let contended = match self.debug_behavior {
            super::DebugBehavior::Discarding => {
                self.stopped(tid)?;
                return Ok(());
            }
            super::DebugBehavior::Faithful => 0,
            super::DebugBehavior::Contended(others) => others,
        };
        #[cfg(test)]
        let phantom = self.sabotage == Some(super::super::world::Sabotage::PhantomArming);
        let thread = self.stopped_mut(tid)?;
        let contended = if thread.tid == thread.tgid {
            0
        } else {
            contended
        };
        let others = contended + thread.debug_held;
        #[cfg(test)]
        if phantom && thread.tid != thread.tgid {
            let registers = thread.debug.clone();
            return self
                .phantom_debug
                .entry(tid)
                .or_insert(registers)
                .poke(index, value, others);
        }
        thread.debug.poke(index, value, others)
    }

    /// Lets something other than the tracer hold `count` more of a
    /// thread's hardware breakpoints, as perf can.
    pub fn hold_debug_slots(&mut self, tid: Tid, count: usize) {
        if let Some(thread) = self.threads.get_mut(&tid) {
            thread.debug_held += count;
        }
    }

    /// `PTRACE_PEEKDATA`, which ignores protections and fails with `EIO`
    /// where nothing is mapped (K-MEM-1).
    pub fn peek(&self, tid: Tid, address: u64) -> Result<u64, Errno> {
        let thread = self.stopped(tid)?;
        let word = self.processes[&thread.tgid]
            .space
            .peek(address)
            .ok_or(Errno::EIO)?;
        #[cfg(test)]
        let word = self.sabotage_read(thread.tgid, address, word);
        Ok(word)
    }

    /// `PTRACE_POKEDATA` (K-MEM-1).
    pub fn poke(&mut self, tid: Tid, address: u64, value: u64) -> Result<(), Errno> {
        let group = self.stopped(tid)?.tgid;
        #[cfg(test)]
        if self.sabotage == Some(super::super::world::Sabotage::LosePokes) {
            return Ok(());
        }
        let space = &mut self
            .processes
            .get_mut(&group)
            .expect("a thread's process exists")
            .space;
        if space.poke(address, value) {
            Ok(())
        } else {
            Err(Errno::EIO)
        }
    }

    /// `PTRACE_GETSIGINFO`.
    pub fn signal_info(&self, tid: Tid) -> Result<SigInfo, Errno> {
        match self.stopped(tid)?.state {
            State::Stopped { info, .. } => Ok(info),
            _ => unreachable!("a stopped thread has stop information"),
        }
    }

    /// `PTRACE_GETREGS`, with `orig_rax`.
    pub fn get_registers_and_call(&self, tid: Tid) -> Result<(Registers, u64), Errno> {
        let thread = self.stopped(tid)?;
        Ok((thread.registers, thread.orig_rax))
    }

    /// `PTRACE_GETEVENTMSG`: the message of the latest event stop, or zero.
    pub fn event_message(&self, tid: Tid) -> Result<u64, Errno> {
        match self.stopped(tid)?.state {
            State::Stopped {
                kind: StopKind::Exit(exit),
                ..
            } => Ok(exit.event_message()),
            State::Stopped {
                kind: StopKind::Event(_, message),
                ..
            } => Ok(message),
            _ => Ok(0),
        }
    }

    /// Whether `tid` finished exiting: it is a zombie, or reaped.
    #[must_use]
    pub fn finished_exiting(&self, tid: Tid) -> bool {
        self.threads
            .get(&tid)
            .is_none_or(|thread| matches!(thread.state, State::Zombie(_)))
    }

    /// The thread group `/proc/<tid>/status` names, until the thread is
    /// reaped.
    #[must_use]
    pub fn thread_group(&self, tid: Tid) -> Option<Tid> {
        self.threads.get(&tid).map(|thread| thread.tgid)
    }

    /// `PTRACE_SETOPTIONS`.
    pub fn set_options(&mut self, tid: Tid, options: Options) -> Result<(), Errno> {
        self.stopped_mut(tid)?.options = options;
        Ok(())
    }

    /// `PTRACE_CONT` with `signal`, delivered only from a
    /// signal-delivery-stop. A thread at an event stop returns from the
    /// system call it stopped in when it next runs; one at its exit event
    /// goes on to exit.
    pub fn resume(
        &mut self,
        tid: Tid,
        signal: Option<i32>,
        single_step: bool,
    ) -> Result<(), Errno> {
        let thread = self.stopped_mut(tid)?;
        thread.single_step = single_step;
        match thread.state {
            State::Stopped {
                kind: StopKind::Signal(_),
                ..
            } => self.resume_with(tid, signal),
            State::Stopped {
                kind: StopKind::Event(..),
                ..
            } => {
                thread.state = State::Running;
                thread.report = None;
            }
            State::Stopped {
                kind: StopKind::Exit(exit),
                ..
            } => self.become_zombie(tid, exit),
            _ => unreachable!("a resumed thread was stopped"),
        }
        Ok(())
    }

    /// `PTRACE_SEIZE` with `options`: the tracer traces the thread, which
    /// runs on (K-SEIZE-1). A thread already traced, or one that has
    /// finished exiting, cannot be seized (K-EXIT-5).
    pub fn seize(&mut self, tid: Tid, options: Options) -> Result<(), Errno> {
        let thread = self.threads.get_mut(&tid).ok_or(Errno::ESRCH)?;
        if thread.traced() {
            return Err(Errno::EPERM);
        }
        if matches!(thread.state, State::Zombie(_)) {
            self.happenings.push(super::Happening::SeizeRefused { tid });
            return Err(Errno::EPERM);
        }
        thread.tracing = Tracing::Seized { interrupted: false };
        thread.options = options;
        Ok(())
    }

    /// `PTRACE_INTERRUPT` (K-INT-1, K-INT-2): a running seized thread stops
    /// before it runs on, and a stopped one as soon as it resumes. One
    /// exiting, at its exit event or a zombie, stays as it is.
    pub fn interrupt(&mut self, tid: Tid) -> Result<(), Errno> {
        let thread = self
            .threads
            .get_mut(&tid)
            .filter(|thread| thread.traced())
            .ok_or(Errno::ESRCH)?;
        if !thread.seized() {
            return Err(Errno::EIO);
        }
        let exiting = matches!(
            thread.state,
            State::Exiting(_)
                | State::Zombie(_)
                | State::Stopped {
                    kind: StopKind::Exit(_),
                    ..
                }
        );
        if !exiting {
            thread.tracing = Tracing::Seized { interrupted: true };
            if thread.is_stopped() {
                self.happenings
                    .push(super::Happening::InterruptWaits { tid });
            }
        }
        Ok(())
    }

    /// `PTRACE_DETACH` with `signal`, delivered only from a
    /// signal-delivery-stop. The thread runs on untraced from where it
    /// stopped, its debug registers as they were (K-FORK-2, K-DR-3).
    pub fn detach(&mut self, tid: Tid, signal: Option<i32>) -> Result<(), Errno> {
        let thread = self.stopped_mut(tid)?;
        thread.tracing = Tracing::Untraced;
        thread.single_step = false;
        thread.options = Options::default();
        thread.report = None;
        let state = thread.state;
        let group = thread.tgid;
        // Only a thread that runs on can meet a trap left in its code.
        let exiting = matches!(
            state,
            State::Stopped {
                kind: StopKind::Exit(_),
                ..
            }
        );
        let planted = if exiting { None } else { self.planted(group) };
        self.happenings
            .push(super::Happening::Released { tid, planted });
        match state {
            State::Stopped {
                kind: StopKind::Signal(_),
                info,
            } => {
                self.threads.get_mut(&tid).expect("the thread").state = State::Running;
                if let Some(signal) = signal {
                    self.act_by_default(tid, SigInfo { signal, ..info });
                }
            }
            State::Stopped {
                kind: StopKind::Event(..),
                ..
            } => self.threads.get_mut(&tid).expect("the thread").state = State::Running,
            State::Stopped {
                kind: StopKind::Exit(exit),
                ..
            } => self.become_zombie(tid, exit),
            _ => unreachable!("a detached thread was stopped"),
        }
        Ok(())
    }

    /// Whether a stopped thread's own pending set holds `SIGTRAP`.
    #[must_use]
    pub fn trap_queued(&self, tid: Tid) -> bool {
        self.threads
            .get(&tid)
            .is_some_and(|thread| thread.pending.contains(super::signals::SIGTRAP))
    }
}
