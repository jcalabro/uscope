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
    waiting_with(fixture, mode, None, Stdio::null()).await
}

/// The same, with the program's standard input and output.
async fn waiting_with(
    fixture: &str,
    mode: &str,
    stdin: Option<Stdio>,
    stdout: Stdio,
) -> (Scenario, u64) {
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
            stdin,
            stdout: Some(stdout),
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

/// `pause` while a step waits for its task ends the step with the pause,
/// as it ends any step. A new step of the task, which no thread runs then, waits
/// for it in turn, and ends at the next line once the task resumes.
#[tokio::test]
async fn a_pause_ends_a_waiting_step_and_a_new_one_waits_again() {
    for fixture in BUILDS {
        let (input, mut opener) = std::io::pipe().expect("a pipe");
        let (output, printed) = std::io::pipe().expect("a pipe");
        let (mut scenario, task) = waiting_with(
            fixture,
            "hold",
            Some(Stdio::from(input)),
            Stdio::from(printed),
        )
        .await;
        let stepping = scenario.start_stepping(StepKind::OverSource).await;
        // Paused once no thread runs the task, the next step begins in it
        // rather than on a thread.
        let held = tokio::task::spawn_blocking(move || {
            std::io::BufRead::lines(std::io::BufReader::new(output))
                .map_while(Result::ok)
                .any(|line| line == "TRUTH\theld")
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(30), held)
                .await
                .expect("the program says it holds the task")
                .expect("the reader"),
            "{fixture}: the program ended without holding the task"
        );
        let paused = scenario.operation("pause", scenario.handle().pause()).await;
        let ended = stepping.await.expect("the step's task").expect("the step");
        assert_eq!(
            (&paused, &ended),
            (&StopReason::Pause, &StopReason::Pause),
            "{fixture}"
        );

        let task = scenario
            .operation("tasks", scenario.handle().tasks(None, 16))
            .await
            .tasks
            .iter()
            .find(|listed| listed.id.number == task)
            .expect("the task is listed")
            .id;
        scenario
            .operation("select task", scenario.handle().select_context(task))
            .await;
        let stepping = scenario.start_stepping(StepKind::OverSource).await;
        std::io::Write::write_all(&mut opener, b"open\n").expect("the program reads");
        let reason = stepping.await.expect("the step's task").expect("the step");
        assert_eq!(
            reason,
            StopReason::Step {
                kind: StepKind::OverSource
            },
            "{fixture}"
        );
        assert_eq!(
            place(&scenario).await,
            ("waiting".to_owned(), line(SOURCE, "// STEP: waiting-after")),
            "{fixture}"
        );
        assert_eq!(crate::steps::stopped_task(&mut scenario).await, task.number);
        scenario.shutdown().await;
    }
}
