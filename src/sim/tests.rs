//! The simulation's gate: fixed seeds over every program and variant, and
//! a check that a seed always names the same run.

use super::marks::{Mark, Marks};
use super::report::FailureKind;
use super::world::Sabotage;
use super::{Corpus, Settings, describe_failure, run};

/// How many fixed seeds the gate runs.
const GATE_SEEDS: u64 = 2000;
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

/// A thread that runs again while a stop is published, behind the
/// controller's back, breaks all-stop, which the oracle catches.
#[test]
fn a_thread_resumed_behind_the_controller_fails_all_stop() {
    assert_eq!(
        first_failure_with(Sabotage::ResumeBehindTheController),
        "all-stop"
    );
}

/// A CPU that executes the program's own instruction under a user's trap
/// lets a hit go unseen, which breakpoint accounting catches.
#[test]
fn a_skipped_trap_fails_breakpoint_accounting() {
    assert_eq!(
        first_failure_with(Sabotage::SkipTraps),
        "breakpoint accounting"
    );
}

/// A kernel that misreports return addresses makes the debugger unwind to
/// callers that are not, which the backtrace oracle catches.
#[test]
fn skewed_return_addresses_fail_the_backtrace_oracle() {
    assert_eq!(
        first_failure_with(Sabotage::SkewReturnAddresses),
        "backtrace"
    );
}

/// A CPU whose single steps run on executes more than a step allows, which
/// the stepping oracle catches.
#[test]
fn late_single_steps_fail_the_stepping_oracle() {
    assert_eq!(first_failure_with(Sabotage::LateSingleSteps), "stepping");
}

/// Runs fixed seeds with `sabotage` until one fails `check`, requiring
/// every failure on the way to be one of `allowed`.
fn some_failure_with(sabotage: Sabotage, check: &str, allowed: &[&str]) {
    some_failure_saying(sabotage, check, "", allowed);
}

/// Like [`some_failure_with`], for a failure of `check` whose message
/// contains `saying`.
fn some_failure_saying(sabotage: Sabotage, check: &str, saying: &str, allowed: &[&str]) {
    let corpus = Corpus::load().expect("load the golden corpus");
    let settings = Settings {
        sabotage: Some(sabotage),
        ..Settings::default()
    };
    for seed in 0..GATE_SEEDS {
        let Some(failure) = run(seed, &corpus, &settings).failure else {
            continue;
        };
        assert_eq!(failure.kind, FailureKind::Debugger, "{failure}");
        assert!(allowed.contains(&failure.check), "{failure}");
        if failure.check == check && failure.message.contains(saying) {
            return;
        }
    }
    panic!("no sabotaged run failed {check} saying {saying:?}");
}

/// A kernel that misreports small numbers on the stack shows variables
/// with wrong values, which the variables oracle catches. Expressions and
/// breakpoint conditions read the same values, and may catch it first.
#[test]
fn skewed_stack_words_fail_the_variables_oracle() {
    some_failure_with(
        Sabotage::SkewSmallStackWords,
        "variables",
        &["variables", "expressions", "breakpoint conditions"],
    );
}

/// The same misreported values make a marker's condition, or what it
/// expects, evaluated as an expression, false where it must hold, and show
/// bytes that memory does not hold, which the expressions oracle catches
/// every way.
#[test]
fn skewed_stack_words_fail_the_expressions_oracle() {
    for saying in ["not true", "which holds", "expected"] {
        some_failure_saying(
            Sabotage::SkewSmallStackWords,
            "expressions",
            saying,
            &["variables", "expressions", "breakpoint conditions"],
        );
    }
}

/// The same misreported values make conditions the client knows hold or
/// fail decide hits wrongly, which the breakpoint-conditions check catches.
#[test]
fn skewed_stack_words_fail_breakpoint_conditions() {
    some_failure_with(
        Sabotage::SkewSmallStackWords,
        "breakpoint conditions",
        &["variables", "expressions", "breakpoint conditions"],
    );
}

/// A kernel whose reads of an unchanged stack value disagree shows a
/// variable with one value and evaluates its name, arithmetic over it, and
/// casts of it with another, which the expressions oracle catches each way.
#[test]
fn flickering_stack_words_fail_the_expressions_oracle() {
    for saying in [
        "as the variables view shows",
        "the exact result",
        "truncated",
    ] {
        some_failure_saying(
            Sabotage::FlickeringStackWords,
            "expressions",
            saying,
            &["variables", "expressions", "breakpoint conditions"],
        );
    }
}

/// A kernel that misreports small numbers in registers shows values held
/// there that the registers do not hold, which the expressions oracle
/// catches.
#[test]
fn skewed_registers_fail_the_expressions_oracle() {
    some_failure_saying(
        Sabotage::SkewSmallRegisters,
        "expressions",
        "which holds",
        &[
            "variables",
            "expressions",
            "breakpoint conditions",
            "backtrace",
            "stepping",
        ],
    );
}

/// A kernel that keeps a thread's debug-register writes in a copy that
/// reads them back arms nothing, so the thread's accesses to watched
/// memory go unseen, which watch accounting catches.
#[test]
fn phantom_debug_registers_fail_watch_accounting() {
    assert_eq!(
        first_failure_with(Sabotage::PhantomArming),
        "watch accounting"
    );
}

/// A CPU that takes no debug exception for a watched access lets the
/// debugger report no hit, which watch accounting catches.
#[test]
fn missed_watch_traps_fail_watch_accounting() {
    assert_eq!(
        first_failure_with(Sabotage::MissWatchTraps),
        "watch accounting"
    );
}

/// A kernel whose reads skip a linked node makes the debugger present a
/// list without one of its elements, which the views oracle catches.
#[test]
fn skipped_linked_nodes_fail_the_views_oracle() {
    some_failure_with(
        Sabotage::SkipLinkedNodes,
        "views",
        &["views", "variables", "expressions", "breakpoint conditions"],
    );
}
