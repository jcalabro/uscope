//! Real async Rust programs, built by the pinned toolchain against the
//! pinned tokio, debugged from start to finish. Each program reports the
//! truth about itself, and assertions compare the debugger against it.

#[path = "../support/mod.rs"]
mod support;

mod cancel;
mod coroutines;
mod drivers;
mod invariants;
mod panics;
mod resume_points;
mod shapes;
mod soak;
mod std_async;
mod steps;
mod stops;
mod workers;
