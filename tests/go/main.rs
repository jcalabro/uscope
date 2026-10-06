//! Real Go programs, built by the pinned toolchain, debugged from start to
//! finish. Each program reports the truth about itself in `TRUTH` lines,
//! computed by the runtime the debugger inspects, and every assertion
//! compares the debugger against it.

#[path = "../support/mod.rs"]
mod support;

mod preemption;
mod stacks;
mod truth;
mod workers;
