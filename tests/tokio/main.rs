//! Real async Rust programs, built by the pinned toolchain against the
//! pinned tokio, debugged from start to finish. Each program reports the
//! truth about itself, and assertions compare the debugger against it.

#[path = "../support/mod.rs"]
mod support;

mod coroutines;
mod invariants;
mod panics;
mod resume_points;
mod std_async;
mod stops;
mod workers;
