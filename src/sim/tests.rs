//! The simulation's gate: fixed seeds over every program and variant, and
//! a check that a seed always names the same run.

use super::marks::{Mark, Marks};
use super::report::FailureKind;
use super::world::Sabotage;
use super::{Corpus, Settings, describe_failure, run};

/// How many fixed seeds the gate runs.
const GATE_SEEDS: u64 = 300;
/// How many seeds the determinism check runs twice.
const DETERMINISM_SEEDS: u64 = 32;

/// Every fixed seed runs a session the debugger handles correctly.
#[test]
fn fixed_seeds_pass_every_oracle() {
    let corpus = Corpus::load().expect("load the golden corpus");
    let settings = Settings::default();
    let mut failures = Vec::new();
    let mut marks = Marks::default();
    for seed in 0..GATE_SEEDS {
        let outcome = run(seed, &corpus, &settings);
        if outcome.failure.is_some() {
            failures.push(describe_failure(&outcome));
        }
        marks.add(&outcome.marks);
    }
    assert!(
        failures.is_empty(),
        "{} of {GATE_SEEDS} seeds failed; the first:\n{}",
        failures.len(),
        failures[0]
    );
    let unreached = Mark::ALL
        .into_iter()
        .filter(|&mark| marks.count(mark) == 0)
        .collect::<Vec<_>>();
    assert!(
        unreached.is_empty(),
        "no fixed seed reached {unreached:?}; counts: {marks:?}"
    );
}

/// A seed names one run: running it again, after other runs in the same
/// process, records the same trace.
#[test]
fn a_seed_always_names_the_same_run() {
    let corpus = Corpus::load().expect("load the golden corpus");
    let settings = Settings::default();
    let first = (0..DETERMINISM_SEEDS)
        .map(|seed| run(seed, &corpus, &settings).fingerprint)
        .collect::<Vec<_>>();
    let again = (0..DETERMINISM_SEEDS)
        .rev()
        .map(|seed| run(seed, &corpus, &settings).fingerprint)
        .rev()
        .collect::<Vec<_>>();
    assert_eq!(first, again);
}

/// Runs fixed seeds with `sabotage` until one fails, returning its
/// failure's check.
fn first_failure_with(sabotage: Sabotage) -> &'static str {
    let corpus = Corpus::load().expect("load the golden corpus");
    let settings = Settings {
        sabotage: Some(sabotage),
        ..Settings::default()
    };
    (0..GATE_SEEDS)
        .find_map(|seed| run(seed, &corpus, &settings).failure)
        .map(|failure| {
            assert_eq!(failure.kind, FailureKind::Debugger, "{failure}");
            failure.check
        })
        .expect("a sabotaged run fails")
}

/// A kernel that loses the debugger's writes leaves traps the controller
/// believes installed missing from memory, which code integrity catches.
#[test]
fn lost_writes_fail_code_integrity() {
    assert_eq!(first_failure_with(Sabotage::LosePokes), "code integrity");
}

/// A waiter that never reaps leaves the client waiting with nothing left
/// to happen, which liveness catches.
#[test]
fn a_deaf_waiter_fails_liveness() {
    assert_eq!(first_failure_with(Sabotage::DeafWaiter), "liveness");
}
