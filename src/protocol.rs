use std::sync::Arc;

use tokio::sync::oneshot;

use crate::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakpointSpec {
    Function(String),
    Address(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Breakpoint { address: u64 },
    Signal(i32),
    Exited(i32),
    Signaled(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferiorState {
    NotRunning,
    Running { pid: u32 },
    Stopped { pid: u32, reason: StopReason },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateSnapshot {
    pub revision: u64,
    pub inferior: InferiorState,
    pub breakpoints: Arc<[u64]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebuggerEvent {
    StateChanged { revision: u64 },
    InferiorLaunched { pid: u32 },
    InferiorStopped { pid: u32, reason: StopReason },
    InferiorExited { pid: u32, reason: StopReason },
    BreakpointsChanged { revision: u64 },
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
