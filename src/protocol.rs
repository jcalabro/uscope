use crate::Result;
use nix::sys::signal::Signal;
use std::sync::mpsc::SyncSender;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakpointSpec {
    Function(String),
    Address(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Breakpoint { address: u64 },
    Signal(Signal),
    Exited(i32),
    Signaled(Signal),
}

pub type Reply<T> = SyncSender<Result<T>>;

pub enum Command {
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
    Shutdown {
        reply: Reply<()>,
    },
}
