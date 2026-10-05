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
    /// The debugger reported a leader's exit at its exit event.
    LeaderExitReported,
    /// An execution of a leader alone ended in its exit.
    LeaderExitEndedExecution,
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
    /// A backtrace showed every call its thread made.
    WholeBacktrace,
    /// A backtrace stopped, saying why, at a frame without call-frame
    /// information.
    TruncatedBacktrace,
    /// A backtrace ended at a caller read from a return address the program
    /// overwrote.
    CorruptCaller,
    /// The stepping oracle judged where a step ended.
    StepJudged,
    /// A source step in unoptimized code passed the exact rules.
    SourceStepExact,
    /// A marker's condition held with the values the debugger read.
    MarkerHeld,
    /// A marker's condition, evaluated as an expression, was true.
    MarkerEvaluated,
    /// A variable's name evaluated to the value the variables view shows.
    NameEvaluated,
    /// An evaluated value's bytes were in memory where the debugger said.
    StorageTrue,
    /// A variable's address evaluated to where the variables view says it
    /// lives.
    AddressEvaluated,
    /// Arithmetic over variables evaluated exactly.
    ArithmeticEvaluated,
    /// What a marker expects, in the debugger's own language, was true.
    ExpectationHeld,
    /// A variable cast to a narrower integer type kept its low bits.
    CastEvaluated,
    /// An ill-typed expression was refused.
    IllTypedRefused,
    /// An evaluated value's bytes were in the register the debugger said.
    RegisterTrue,
    /// The client changed a breakpoint's hit condition or condition.
    BreakpointAmended,
    /// A breakpoint counted a hit that did not stop.
    HitDeclined,
    /// A breakpoint stopped at a hit whose condition the client knew held.
    ConditionHeld,
    /// A breakpoint logged a message instead of stopping.
    HitLogged,
    /// The debugger armed a watchpoint.
    WatchAdded,
    /// The debug registers refused a watchpoint, discarding writes or with
    /// their slots busy.
    WatchRefused,
    /// A watchpoint stopped the program.
    WatchpointStop,
    /// Watch accounting judged a reported hit.
    WatchHit,
    /// A thread other than the first reported a watchpoint hit.
    WatchHitOnAnotherThread,
    /// A store left watched bytes as they were, which a watchpoint on
    /// stores reports and one on changes does not.
    UnchangedStore,
    /// A new thread could not be armed, its slots busy.
    WatchArmFailed,
    /// The debugger refused to run while a thread could not be armed.
    RunRefusedUnarmed,
    /// A traced program forked.
    Forked,
    /// A thread other than a process's first forked.
    ForkedFromThread,
    /// The debugger released a fork child.
    ForkChildReleased,
    /// The debugger released a fork child whose parent had exited.
    ReleasedAfterParentExit,
    /// A program reaped a child it forked.
    ChildReaped,
    /// Init reaped a child that outlived its parent.
    OrphanReaped,
    /// SIGKILL from outside landed right after a fork.
    KilledNearFork,
    /// The client attached to a program running untraced.
    Attached,
    /// Attaching found a thread that had finished exiting, which it could
    /// not seize.
    SeizeRefused,
    /// An interrupt reached a thread already stopped.
    InterruptWaited,
    /// A thread or process a seized thread created started in an
    /// interrupt's stop.
    SeizedChildStarted,
    /// The debugger released the program it attached to.
    Detached,
    /// A released program ran on to its own end, which transparency judged.
    FinishedAfterDetach,
}

impl Mark {
    pub const ALL: [Self; 68] = [
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
        Self::LeaderExitReported,
        Self::LeaderExitEndedExecution,
        Self::GroupExitEndedStop,
        Self::CoHit,
        Self::ThreadContinued,
        Self::ThreadSelected,
        Self::PreemptedCall,
        Self::ReapedInsideCall,
        Self::KilledAtStep,
        Self::KilledNearClone,
        Self::KilledInsideCall,
        Self::WholeBacktrace,
        Self::TruncatedBacktrace,
        Self::CorruptCaller,
        Self::StepJudged,
        Self::SourceStepExact,
        Self::MarkerHeld,
        Self::MarkerEvaluated,
        Self::NameEvaluated,
        Self::StorageTrue,
        Self::AddressEvaluated,
        Self::ArithmeticEvaluated,
        Self::ExpectationHeld,
        Self::CastEvaluated,
        Self::IllTypedRefused,
        Self::RegisterTrue,
        Self::BreakpointAmended,
        Self::HitDeclined,
        Self::ConditionHeld,
        Self::HitLogged,
        Self::WatchAdded,
        Self::WatchRefused,
        Self::WatchpointStop,
        Self::WatchHit,
        Self::WatchHitOnAnotherThread,
        Self::UnchangedStore,
        Self::WatchArmFailed,
        Self::RunRefusedUnarmed,
        Self::Forked,
        Self::ForkedFromThread,
        Self::ForkChildReleased,
        Self::ReleasedAfterParentExit,
        Self::ChildReaped,
        Self::OrphanReaped,
        Self::KilledNearFork,
        Self::Attached,
        Self::SeizeRefused,
        Self::InterruptWaited,
        Self::SeizedChildStarted,
        Self::Detached,
        Self::FinishedAfterDetach,
    ];
}

/// How many times a run reached each mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marks([u64; Mark::ALL.len()]);

impl Default for Marks {
    fn default() -> Self {
        Self([0; Mark::ALL.len()])
    }
}

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
