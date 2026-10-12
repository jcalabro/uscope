//! The system call engines (`plans/syscalls.html`): each turns what it
//! sees into the neutral events of `crate::syscalls`.

pub mod bpf;
pub mod info;
pub mod launch;
pub mod ptrace;
pub mod seccomp;

use crate::syscalls::stats::Folded;
use crate::syscalls::{InFlight, TraceEvent};

/// Where an engine sends what it sees, on the engine's own thread.
pub trait Sink {
    /// One event, in the order the engine saw it.
    fn event(&mut self, event: TraceEvent);

    /// A running process is now traced, every thread of it.
    fn attached(&mut self, _process: u32) {}

    /// The calls threads have been blocked in since before `since`, read
    /// every so often while the trace runs.
    fn in_flight(&mut self, _calls: &[InFlight]) {}

    /// Nanoseconds a call must have been blocked to be reported in flight;
    /// `None` asks for no reports.
    fn in_flight_after(&self) -> Option<u64> {
        None
    }

    /// Statistics an engine counted itself, without reporting the calls,
    /// once the trace ends.
    fn counted(&mut self, _stats: Folded) {}
}

pub use ptrace::{Finished, Target, TraceOptions, monotonic};
