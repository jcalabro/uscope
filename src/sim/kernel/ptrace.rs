//! The tracer's ptrace requests. Each answers as Linux does, in terms of
//! the K-* rules; above all K-WAIT-1, that a request to a thread not in a
//! ptrace-stop fails with `ESRCH`.

use nix::errno::Errno;
use nix::libc;

use super::{Kernel, Options, SigInfo, State, StopKind, Thread, Tid};
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

    /// `PTRACE_PEEKDATA`, which ignores protections and fails with `EIO`
    /// where nothing is mapped (K-MEM-1).
    pub fn peek(&self, tid: Tid, address: u64) -> Result<u64, Errno> {
        let thread = self.stopped(tid)?;
        self.processes[&thread.tgid]
            .space
            .peek(address)
            .ok_or(Errno::EIO)
    }

    /// `PTRACE_POKEDATA` (K-MEM-1).
    pub fn poke(&mut self, tid: Tid, address: u64, value: u64) -> Result<(), Errno> {
        let group = self.stopped(tid)?.tgid;
        #[cfg(test)]
        if self.lose_pokes {
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

    /// `PTRACE_GETEVENTMSG`: the message of the event stop, or zero.
    pub fn event_message(&self, tid: Tid) -> Result<u64, Errno> {
        match self.stopped(tid)?.state {
            State::Stopped {
                kind: StopKind::Exit(exit),
                ..
            } => Ok(exit.event_message()),
            _ => Ok(0),
        }
    }

    /// `PTRACE_SETOPTIONS`.
    pub fn set_options(&mut self, tid: Tid, options: Options) -> Result<(), Errno> {
        self.stopped_mut(tid)?.options = options;
        Ok(())
    }

    /// `PTRACE_CONT` with `signal`, delivered only from a
    /// signal-delivery-stop. A thread at its exit event goes on to exit.
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
                kind: StopKind::Exit(exit),
                ..
            } => self.become_zombie(tid, exit),
            _ => unreachable!("a resumed thread was stopped"),
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
