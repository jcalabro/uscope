use std::sync::Arc;

use tokio::sync::oneshot;

use crate::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakpointSpec {
    Function(String),
    Address(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessId(u64);

impl ProcessId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExceptionInfo {
    pub code: u64,
    pub description: Arc<str>,
}

impl ExceptionInfo {
    pub fn new(code: u64, description: impl Into<Arc<str>>) -> Self {
        Self {
            code,
            description: description.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitStatus {
    Code(i64),
    Terminated(ExceptionInfo),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    Breakpoint { address: u64 },
    Exception(ExceptionInfo),
    Exited(ExitStatus),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InferiorState {
    NotRunning,
    Running {
        process_id: ProcessId,
    },
    Stopped {
        process_id: ProcessId,
        reason: StopReason,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateSnapshot {
    pub revision: u64,
    pub inferior: InferiorState,
    pub breakpoints: Arc<[u64]>,
}

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
        address: u64,
        relocate: bool,
        reply: Reply<()>,
    },
    Launch {
        reply: Reply<StopReason>,
    },
    Continue {
        reply: Reply<StopReason>,
    },
    ReadWord {
        address: u64,
        reply: Reply<u64>,
    },
    Relocate {
        link_address: u64,
        reply: Reply<u64>,
    },
    Snapshot {
        reply: Reply<StateSnapshot>,
    },
    Shutdown {
        reply: Reply<()>,
    },
}
