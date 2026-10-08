//! Steps whose future goes away while they wait for it: its task is
//! aborted, or its runtime shuts down, and the step says the task was
//! cancelled; or the code that awaits it drops it, as a `select!` and a
//! timeout do, and the step goes on to that code's next line, saying the
//! future was dropped.

use std::process::Stdio;

use uscope::{BreakpointSpec, LaunchOptions, LineNumber, StepKind, StopReason, TaskEnding};

use crate::invariants::checked;
use crate::stops::{line, place};
use crate::support::Scenario;

const BUILDS: [&str; 1] = ["tokio-cancel-o0"];
const SOURCE: &str = "cancel/src/main.rs";

/// The fixture in `mode`, stopped where its task first arrives at the
/// await that never finishes, and the task's number.
async fn waiting(fixture: &str, mode: &str) -> (Scenario, u64) {
    let mut scenario = checked(fixture);
    let breakpoint = scenario
        .add_breakpoint_spec(BreakpointSpec::Source {
            path: SOURCE.into(),
            line: LineNumber::new(line(SOURCE, "// AWAIT: waiting")).expect("one-based"),
        })
        .await;
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            arguments: vec![mode.into()],
            stdout: Some(Stdio::null()),
            ..LaunchOptions::default()
        })
        .await;
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{fixture} {mode}: {reason:?}"
    );
    scenario.remove_breakpoint(breakpoint.id).await;
    let task = crate::steps::stopped_task(&mut scenario).await;
    (scenario, task)
}

/// A step that waits for a future whose task is aborted, or whose runtime
/// shuts down, ends as the runtime drops the future, saying the task was
/// cancelled.
#[tokio::test]
async fn a_step_whose_task_is_cancelled_says_so() {
    for fixture in BUILDS {
        for mode in ["abort", "shutdown"] {
            for kind in [StepKind::OverSource, StepKind::Out] {
                let context = format!("{fixture} {mode} {kind:?}");
                let (mut scenario, task) = waiting(fixture, mode).await;
                let reason = scenario.step_to_stop(kind).await;
                assert!(
                    matches!(
                        reason,
                        StopReason::TaskEnded {
                            kind: ended,
                            task: cancelled,
                            ending: TaskEnding::Cancelled,
                        } if ended == kind && cancelled.number == task
                    ),
                    "{context}: {reason:?}"
                );
                scenario.shutdown().await;
            }
        }
    }
}

/// A step that waits for a future that the code awaiting it drops, as a
/// `select!` does once another branch finishes and a timeout does once it
/// elapses, goes on in that code to its next line, and says the future
/// was dropped: the branch that finished, and the line after the timeout.
#[tokio::test]
async fn a_step_whose_future_is_dropped_ends_on_the_droppers_next_line() {
    for fixture in BUILDS {
        for (mode, function, after) in [
            ("select", "selecting", "// STEP: select-other"),
            ("timeout", "timing", "// STEP: timeout-after"),
        ] {
            for kind in [StepKind::OverSource, StepKind::Out] {
                let context = format!("{fixture} {mode} {kind:?}");
                let (mut scenario, task) = waiting(fixture, mode).await;
                let reason = scenario.step_to_stop(kind).await;
                assert_eq!(reason, StopReason::FutureDropped { kind }, "{context}");
                assert_eq!(
                    place(&scenario).await,
                    (function.to_owned(), line(SOURCE, after)),
                    "{context}"
                );
                assert_eq!(
                    crate::steps::stopped_task(&mut scenario).await,
                    task,
                    "{context}"
                );
                scenario.shutdown().await;
            }
        }
    }
}
