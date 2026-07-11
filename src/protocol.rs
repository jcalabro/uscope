use std::sync::Arc;

use tokio::sync::oneshot;

use crate::{
    Backtrace, BreakpointLocation, LoadedModule, RegisterSnapshot, Result, VirtualAddress,
};

/// A user-facing request for a logical breakpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakpointSpec {
    /// Break at the uniquely named function.
    Function(String),
    /// Break at an absolute process virtual address.
    Address(VirtualAddress),
}

/// Identifies an inferior process within a debug session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessId(u64);

impl ProcessId {
    /// Creates a process identifier from its numeric representation.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric process identifier.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Platform-neutral information about an exception that stopped an inferior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExceptionInfo {
    /// The platform-defined exception code.
    pub code: u64,
    /// A human-readable description of the exception.
    pub description: Arc<str>,
}

impl ExceptionInfo {
    /// Creates exception information from a platform code and description.
    pub fn new(code: u64, description: impl Into<Arc<str>>) -> Self {
        Self {
            code,
            description: description.into(),
        }
    }
}

/// Describes how an inferior process exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitStatus {
    /// The process returned an exit code.
    Code(i64),
    /// The process was terminated by an exception.
    Terminated(ExceptionInfo),
}

/// Describes why execution stopped or completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// Execution reached an installed breakpoint.
    Breakpoint { address: VirtualAddress },
    /// Execution stopped because of an exception.
    Exception(ExceptionInfo),
    /// The inferior exited.
    Exited(ExitStatus),
}

/// The externally observable state of the inferior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InferiorState {
    /// No inferior process exists.
    NotRunning,
    /// The inferior is starting or running.
    Running {
        /// The running process.
        process_id: ProcessId,
    },
    /// The inferior is stopped and may be inspected.
    Stopped {
        /// The stopped process.
        process_id: ProcessId,
        /// The reason execution stopped.
        reason: StopReason,
    },
}

/// An immutable view of debugger state at one revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateSnapshot {
    /// The debugger revision represented by this snapshot.
    pub revision: u64,
    /// The current inferior state.
    pub inferior: InferiorState,
    /// The logical breakpoints requested by clients.
    pub breakpoints: Arc<[BreakpointLocation]>,
}

/// A state or lifecycle event emitted by the debugger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebuggerEvent {
    StateChanged {
        revision: u64,
    },
    InferiorLaunched {
        process_id: ProcessId,
    },
    InferiorStopped {
        process_id: ProcessId,
        reason: StopReason,
    },
    InferiorExited {
        process_id: ProcessId,
        status: ExitStatus,
    },
    BreakpointsChanged {
        revision: u64,
    },
}

pub type Reply<T> = oneshot::Sender<Result<T>>;

pub enum Request {
    AddBreakpoint {
        location: BreakpointLocation,
        reply: Reply<()>,
    },
    Launch {
        reply: Reply<StopReason>,
    },
    Continue {
        reply: Reply<StopReason>,
    },
    ReadWord {
        address: VirtualAddress,
        reply: Reply<u64>,
    },
    LoadedModule {
        reply: Reply<LoadedModule>,
    },
    StoppedLocation {
        reply: Reply<(LoadedModule, VirtualAddress)>,
    },
    Snapshot {
        reply: Reply<StateSnapshot>,
    },
    Backtrace {
        reply: Reply<Backtrace>,
    },
    Registers {
        reply: Reply<RegisterSnapshot>,
    },
    Shutdown {
        reply: Reply<()>,
    },
}
