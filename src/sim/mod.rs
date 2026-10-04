//! Deterministic simulation of debugging sessions; see `plans/simulator.md`.
//!
//! A run drives the real controller and `DebuggerHandle` against a simulated
//! Linux kernel and x86-64 CPU, so that one seed names one complete,
//! reproducible session.
//!
//! - [`world`]: the step loop that schedules every action of a session, and
//!   `machine`, what its actions and the controller's calls reach.
//! - [`kernel`], [`cpu`], [`memory`], [`loader`]: the simulated machine.
//! - [`client`]: the simulated user, driving `DebuggerHandle`.
//! - [`oracles`] and `audit`: checks of the debugger against ground truth.
//! - [`choices`], [`swarm`], [`schedule`]: where every random choice comes
//!   from, and how the next action is chosen.
//! - [`faults`]: what the rest of the machine does to a session.
//! - [`corpus`]: the golden programs.
//! - [`report`]: traces, fingerprints, and failures.

mod audit;
pub mod choices;
mod client;
#[cfg(test)]
mod conformance;
pub mod corpus;
pub mod cpu;
pub mod faults;
pub mod kernel;
pub mod loader;
mod machine;
pub mod marks;
pub mod memory;
mod oracles;
pub mod report;
pub mod schedule;
pub mod swarm;
#[cfg(test)]
mod tests;
pub mod world;

use std::fmt::Write as _;

pub use corpus::Corpus;
pub use world::{Outcome, Settings, run};

/// A failed run, written to be understood without rerunning it.
#[must_use]
pub fn describe_failure(outcome: &Outcome) -> String {
    let mut text = String::new();
    if let Some(failure) = &outcome.failure {
        let _ = writeln!(
            text,
            "SIM FAILURE  {}  seed={:#018x}",
            failure.kind, outcome.seed
        );
        let _ = writeln!(text, "program  {} {:?}", outcome.program, outcome.arguments);
        let _ = writeln!(text, "swarm    {}", outcome.swarm);
        let _ = writeln!(
            text,
            "{}  #{} {}",
            failure.check, failure.step, failure.message
        );
    }
    if let Some(plan) = outcome.unfired {
        let _ = writeln!(text, "fault    {plan} never fired");
    }
    if outcome.dropped > 0 {
        let _ = writeln!(text, "  ... {} earlier lines", outcome.dropped);
    }
    for line in &outcome.trace {
        let _ = writeln!(text, "  {line}");
    }
    let _ = writeln!(
        text,
        "replay   just sim-seed {:#x} --fingerprint {:#x}",
        outcome.seed, outcome.fingerprint
    );
    text
}
