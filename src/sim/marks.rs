//! Coverage marks: interesting states a run reached, counted so that the
//! gate can require its seeds to reach every one. A sweep that never
//! reaches what it claims to test passes for nothing.

/// One interesting state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// A launch stopped at the program's first instruction.
    EntryStop,
    /// A breakpoint stopped the program.
    BreakpointStop,
    /// A step completed.
    StepStop,
    /// A pause stopped the running program.
    PauseStop,
    /// The program ran to its own exit.
    ProgramExited,
    /// The client killed the program while it ran.
    KilledRunning,
    /// The client killed the program while it was stopped.
    KilledStopped,
    /// The client launched the program a second time.
    Relaunched,
    /// The client added or removed a breakpoint while the program ran.
    EditWhileRunning,
    /// A memory read covered a trap the debugger planted.
    ReadOverTrap,
    /// A request naming a stale stop was refused.
    StaleStopRefused,
    /// Stepping out of the outermost frame was refused.
    StepOutRefused,
    /// The client's event receiver fell behind.
    ClientLagged,
    /// The waiter held a status because the controller's queue was full.
    QueueFull,
    /// A program created a thread.
    ThreadCreated,
    /// The debugger reported a thread exiting while its process ran on.
    ThreadExited,
    /// A group leader exited while its other threads ran on.
    LeaderExitedAlone,
    /// A program's `exit_group` took a thread out of a ptrace-stop.
    GroupExitEndedStop,
    /// A published stop found two threads at breakpoints.
    CoHit,
    /// The client resumed one thread alone.
    ThreadContinued,
    /// The client selected a thread other than the one that stopped.
    ThreadSelected,
    /// A thread ran between two of the controller's calls into the kernel.
    PreemptedCall,
    /// The waiter reaped a status between two of the controller's calls.
    ReapedInsideCall,
    /// A planned SIGKILL from outside landed at a chosen action.
    KilledAtStep,
    /// A planned SIGKILL from outside landed right after a clone.
    KilledNearClone,
    /// A planned SIGKILL from outside landed between two ptrace requests.
    KilledInsideCall,
}

impl Mark {
    pub const ALL: [Self; 26] = [
        Self::EntryStop,
        Self::BreakpointStop,
        Self::StepStop,
        Self::PauseStop,
        Self::ProgramExited,
        Self::KilledRunning,
        Self::KilledStopped,
        Self::Relaunched,
        Self::EditWhileRunning,
        Self::ReadOverTrap,
        Self::StaleStopRefused,
        Self::StepOutRefused,
        Self::ClientLagged,
        Self::QueueFull,
        Self::ThreadCreated,
        Self::ThreadExited,
        Self::LeaderExitedAlone,
        Self::GroupExitEndedStop,
        Self::CoHit,
        Self::ThreadContinued,
        Self::ThreadSelected,
        Self::PreemptedCall,
        Self::ReapedInsideCall,
        Self::KilledAtStep,
        Self::KilledNearClone,
        Self::KilledInsideCall,
    ];
}

/// How many times a run reached each mark.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Marks([u64; Mark::ALL.len()]);

impl Marks {
    pub const fn hit(&mut self, mark: Mark) {
        self.0[mark as usize] += 1;
    }

    #[must_use]
    pub const fn count(&self, mark: Mark) -> u64 {
        self.0[mark as usize]
    }

    /// Adds another run's counts to these.
    pub fn add(&mut self, other: &Self) {
        for (total, count) in self.0.iter_mut().zip(other.0) {
            *total += count;
        }
    }
}
