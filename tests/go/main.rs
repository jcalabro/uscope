//! Real Go programs, built by the pinned toolchain, debugged from start to
//! finish. Each program reports the truth about itself in `TRUTH` lines,
//! computed by the runtime the debugger inspects, and every assertion
//! compares the debugger against it.

#[path = "../support/mod.rs"]
mod support;

mod attach;
mod cgo;
mod cores;
mod corrupted;
mod defers;
mod failing;
mod hosted;
mod invariants;
mod preemption;
mod ranges;
mod scale;
mod server;
mod siblings;
mod stacks;
mod steps;
mod stops;
mod stripped;
mod truth;
mod watches;
mod workers;
