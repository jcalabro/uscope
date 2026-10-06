//! The tracer's ptrace requests. Each answers as Linux does, in terms of
//! the K-* rules; above all K-WAIT-1, that a request to a thread not in a
//! ptrace-stop fails with `ESRCH`.

use nix::errno::Errno;
use nix::libc;

use super::{
    DebugBehavior, Happening, Kernel, Options, SigInfo, State, StopKind, Thread, Tid, Tracing,
    WaitStatus,
};
use crate::sim::cpu::Registers;
#[cfg(test)]
use crate::sim::world::Sabotage;

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
    /// The thread a request addresses, which the caller must trace, in a
    /// ptrace-stop.
    fn stopped(&self, tid: Tid) -> Result<&Thread, Errno> {
        let caller = self.caller();
        self.threads
            .get(&tid)
            .filter(|thread| thread.tracer() == Some(caller) && thread.is_stopped())
            .ok_or(Errno::ESRCH)
    }

    fn stopped_mut(&mut self, tid: Tid) -> Result<&mut Thread, Errno> {
        let caller = self.caller();
        self.threads
            .get_mut(&tid)
            .filter(|thread| thread.tracer() == Some(caller) && thread.is_stopped())
            .ok_or(Errno::ESRCH)
    }

    /// `PTRACE_GETREGS`.
    pub fn get_registers(&self, tid: Tid) -> Result<Registers, Errno> {
        let registers = self.stopped(tid)?.registers;
        #[cfg(test)]
        let registers = self.reported(registers);
        Ok(registers)
    }

    /// `PTRACE_SETREGS`.
    pub fn set_registers(&mut self, tid: Tid, registers: Registers) -> Result<(), Errno> {
        #[cfg(test)]
        let registers = {
            // A value the sabotage reported writes back what the register
            // held, so the program runs on unchanged.
            let held = self.stopped(tid)?.registers;
            let reported = self.reported(held);
            let mut registers = registers;
            for (index, value) in registers.general.iter_mut().enumerate() {
                if *value == reported.general[index] {
                    *value = held.general[index];
                }
            }
            registers
        };
        self.stopped_mut(tid)?.registers = registers;
        Ok(())
    }

    /// Registers as the kernel reports them: under
    /// [`Sabotage::SkewSmallRegisters`],
    /// general registers other than the stack and frame pointers that hold
    /// small numbers other than zero read one greater.
    #[cfg(test)]
    fn reported(&self, mut registers: Registers) -> Registers {
        if self.sabotage == Some(Sabotage::SkewSmallRegisters) {
            for (index, value) in registers.general.iter_mut().enumerate() {
                if index != crate::sim::cpu::RSP
                    && index != crate::sim::cpu::RBP
                    && (1..0x1000).contains(value)
                {
                    *value += 1;
                }
            }
        }
        registers
    }

    /// `PTRACE_PEEKUSER` of a debug register (K-DR-3, K-DR-4).
    pub fn peek_debug(&self, tid: Tid, index: usize) -> Result<u64, Errno> {
        let thread = self.stopped(tid)?;
        #[cfg(test)]
        if let Some(phantom) = self.phantom_debug.get(&tid) {
            return Ok(phantom.peek(index));
        }
        match self.debug_behavior {
            DebugBehavior::Discarding => Ok(0),
            _ => Ok(thread.debug.peek(index)),
        }
    }

    /// `PTRACE_POKEUSER` of a debug register (K-DR-4).
    pub fn poke_debug(&mut self, tid: Tid, index: usize, value: u64) -> Result<(), Errno> {
        let contended = match self.debug_behavior {
            DebugBehavior::Discarding => {
                self.stopped(tid)?;
                return Ok(());
            }
            DebugBehavior::Faithful => 0,
            DebugBehavior::Contended(others) => others,
        };
        #[cfg(test)]
        let phantom = self.sabotage == Some(Sabotage::PhantomArming);
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
    #[cfg(test)]
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
        if self.sabotage == Some(Sabotage::LosePokes) {
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

    /// `PTRACE_GETSIGMASK`. No simulated program blocks a signal.
    pub fn signal_mask(&self, tid: Tid) -> Result<u64, Errno> {
        self.stopped(tid)?;
        Ok(0)
    }

    /// `PTRACE_SETSIGMASK`, which can only leave every signal unblocked:
    /// blocking is not modeled.
    pub fn set_signal_mask(&mut self, tid: Tid, mask: u64) -> Result<(), Errno> {
        self.stopped(tid)?;
        if mask != 0 {
            self.gap(format!("blocking signals {mask:#x}"));
            return Err(Errno::ENOSYS);
        }
        Ok(())
    }

    /// `PTRACE_GETREGS`, with `orig_rax`.
    pub fn get_registers_and_call(&self, tid: Tid) -> Result<(Registers, u64), Errno> {
        let thread = self.stopped(tid)?;
        let registers = thread.registers;
        #[cfg(test)]
        let registers = self.reported(registers);
        Ok((registers, thread.orig_rax))
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
    /// goes on to exit. Resuming a thread from a group-stop while its
    /// process is still stopped is not modeled.
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
                kind: StopKind::Group(_),
                ..
            } => {
                let group = thread.tgid;
                if self.processes[&group].stopped.is_some() {
                    self.gap("resuming a thread of a stopped process");
                    return Err(Errno::ENOSYS);
                }
                let thread = self.threads.get_mut(&tid).expect("the thread");
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

    /// `PTRACE_SEIZE` with `options`: the caller traces the thread, which
    /// runs on (K-SEIZE-1), or, in a job-control stop, reports a group-stop
    /// (K-STOP-2). A thread already traced, or one that has finished
    /// exiting, cannot be seized (K-EXIT-5).
    pub fn seize(&mut self, tid: Tid, options: Options) -> Result<(), Errno> {
        let tracer = self.caller();
        let thread = self.threads.get_mut(&tid).ok_or(Errno::ESRCH)?;
        if thread.traced() {
            return Err(Errno::EPERM);
        }
        if matches!(thread.state, State::Zombie(_)) {
            self.happenings.push(Happening::SeizeRefused { tid });
            return Err(Errno::EPERM);
        }
        thread.tracing = Tracing::Seized {
            tracer,
            interrupted: false,
        };
        thread.options = options;
        if thread.state == State::JobStopped {
            let signal = self.processes[&thread.tgid]
                .stopped
                .expect("a stopped thread's process is stopped");
            thread.enter_stop(
                StopKind::Group(signal),
                SigInfo::group_stop(tid, signal),
                WaitStatus::GroupStop(tid, signal),
            );
        }
        Ok(())
    }

    /// `PTRACE_INTERRUPT` (K-INT-1, K-INT-2): a running seized thread stops
    /// before it runs on, and a stopped one as soon as it resumes. One
    /// exiting, at its exit event or a zombie, stays as it is.
    pub fn interrupt(&mut self, tid: Tid) -> Result<(), Errno> {
        let caller = self.caller();
        let thread = self
            .threads
            .get_mut(&tid)
            .filter(|thread| thread.tracer() == Some(caller))
            .ok_or(Errno::ESRCH)?;
        if !thread.seized() {
            return Err(Errno::EIO);
        }
        if !thread.state.exiting() {
            thread.tracing = Tracing::Seized {
                tracer: caller,
                interrupted: true,
            };
            if thread.is_stopped() {
                self.happenings.push(Happening::InterruptWaits { tid });
            }
        }
        Ok(())
    }

    /// `PTRACE_DETACH` with `signal`, delivered only from a
    /// signal-delivery-stop. The thread runs on untraced from where it
    /// stopped, its debug registers as they were (K-FORK-2, K-DR-3), or,
    /// its process stopped, stops again (K-STOP-2).
    pub fn detach(&mut self, tid: Tid, signal: Option<i32>) -> Result<(), Errno> {
        let tracer = self.caller();
        #[cfg(test)]
        let forget = self.sabotage == Some(Sabotage::ForgetStopRequests);
        let thread = self.stopped_mut(tid)?;
        #[cfg(test)]
        if forget {
            thread.pending.remove(super::signals::SIGSTOP);
        }
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
        self.happenings.push(Happening::Released {
            tid,
            tracer,
            planted,
        });
        match state {
            State::Stopped {
                kind: StopKind::Signal(_),
                ..
            } => {
                self.threads.get_mut(&tid).expect("the thread").state = State::Running;
                if let Some(signal) = signal {
                    self.act_by_default(tid, signal);
                }
            }
            State::Stopped {
                kind: StopKind::Event(..) | StopKind::Group(_),
                ..
            } => self.threads.get_mut(&tid).expect("the thread").state = State::Running,
            State::Stopped {
                kind: StopKind::Exit(exit),
                ..
            } => self.become_zombie(tid, exit),
            _ => unreachable!("a detached thread was stopped"),
        }
        let stopped = self
            .processes
            .get(&group)
            .is_some_and(|process| process.stopped.is_some());
        if let Some(thread) = self.threads.get_mut(&tid)
            && thread.state == State::Running
            && stopped
        {
            thread.state = State::JobStopped;
        }
        Ok(())
    }

    /// Whether `signal` is pending for a stopped thread: in its own set,
    /// or in its process's when `process_wide`.
    #[must_use]
    pub fn signal_queued(&self, tid: Tid, signal: i32, process_wide: bool) -> bool {
        #[cfg(test)]
        if self.sabotage == Some(Sabotage::HideQueuedContinue) && signal == libc::SIGCONT {
            return false;
        }
        let Some(thread) = self.threads.get(&tid) else {
            return false;
        };
        if process_wide {
            self.processes
                .get(&thread.tgid)
                .is_some_and(|process| process.shared.contains(signal))
        } else {
            thread.pending.contains(signal)
        }
    }
}
