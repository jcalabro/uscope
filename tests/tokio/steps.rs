//! Steps through tokio tasks' async functions, across awaits that are
//! pending while the other tasks run the same code, on a multi-thread and
//! a current-thread runtime.

use std::process::Stdio;

use uscope::{
    BreakpointSpec, InferiorState, LaunchOptions, LineNumber, StepKind, StopReason, TaskEnding,
    ThreadActivity, VariableKind,
};

use crate::invariants::checked;
use crate::stops::{integer, line, place};
use crate::support::Scenario;

const BUILDS: [&str; 1] = ["tokio-steps-o0"];
const SOURCE: &str = "steps/src/main.rs";

/// The runtimes the fixture runs its tasks on, by their argument.
const MODES: [Option<&str>; 2] = [None, Some("current")];

fn at(marker: &str) -> BreakpointSpec {
    BreakpointSpec::Source {
        path: SOURCE.into(),
        line: LineNumber::new(line(SOURCE, marker)).expect("one-based"),
    }
}

/// The fixture in `mode`, stopped at the first arrival at `marker`, whose
/// breakpoint is then removed so that only steps stop it.
async fn stopped_once(fixture: &str, mode: Option<&str>, marker: &str) -> Scenario {
    let mut scenario = checked(fixture);
    let breakpoint = scenario.add_breakpoint_spec(at(marker)).await;
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            arguments: mode.into_iter().map(Into::into).collect(),
            stdout: Some(Stdio::null()),
            ..LaunchOptions::default()
        })
        .await;
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{fixture} {mode:?}: {reason:?}"
    );
    scenario.remove_breakpoint(breakpoint.id).await;
    scenario
}

/// The number of the task the stopped thread runs.
pub async fn stopped_task(scenario: &mut Scenario) -> u64 {
    let snapshot = scenario.snapshot().await;
    let InferiorState::Stopped { thread_id, .. } = snapshot.inferior else {
        panic!("not stopped: {:?}", snapshot.inferior);
    };
    let thread = snapshot
        .threads
        .iter()
        .find(|thread| thread.id == thread_id)
        .expect("the stopped thread is listed");
    match &thread.activity {
        Some(ThreadActivity::Task { task, .. }) => task.number,
        other => panic!("thread {thread_id} runs no task: {other:?}"),
    }
}

/// A step of `kind` that completes, and the function and line it ends at.
async fn step(scenario: &mut Scenario, kind: StepKind) -> (String, u64) {
    let reason = scenario.step_to_stop(kind).await;
    assert_eq!(reason, StopReason::Step { kind });
    place(scenario).await
}

/// `next` over an await that is pending, while the other tasks run the
/// same function and arrive where it resumes, ends at the next line in
/// the same task, once its gate opens: in the async function the step
/// began in, and in its caller for a pending await the function itself
/// passes on.
#[tokio::test]
async fn next_over_a_pending_await_ends_on_the_next_line_of_its_task() {
    for fixture in BUILDS {
        for mode in MODES {
            for (marker, function, after) in [
                ("// AWAIT: inner", "inner", "// STEP: inner-after"),
                ("// AWAIT: outer", "outer", "// STEP: outer-after"),
            ] {
                let context = format!("{fixture} {mode:?} {marker}");
                let mut scenario = stopped_once(fixture, mode, marker).await;
                let task = stopped_task(&mut scenario).await;
                assert_eq!(
                    integer(&scenario, "me").await,
                    Some(i128::from(task)),
                    "{context}"
                );
                assert_eq!(
                    step(&mut scenario, StepKind::OverSource).await,
                    (function.to_owned(), line(SOURCE, after)),
                    "{context}"
                );
                assert_eq!(stopped_task(&mut scenario).await, task, "{context}");
                // The await keeps no copy of the argument, whose slot on
                // the stack another poll may have written.
                assert_eq!(integer(&scenario, "me").await, None, "{context}");
                scenario.shutdown().await;
            }
        }
    }
}

/// `next` over an await in a loop, which yields once each time round,
/// stops at the loop's lines each time round, in the same task.
#[tokio::test]
async fn next_goes_round_a_loop_of_awaits() {
    for fixture in BUILDS {
        for mode in MODES {
            let context = format!("{fixture} {mode:?}");
            let mut scenario = stopped_once(fixture, mode, "// STEP: round").await;
            let task = stopped_task(&mut scenario).await;
            let mut rounds = Vec::new();
            loop {
                let (function, at) = step(&mut scenario, StepKind::OverSource).await;
                assert_eq!(function, "rounds", "{context}: {rounds:?}");
                assert_eq!(stopped_task(&mut scenario).await, task, "{context}");
                if at == line(SOURCE, "// STEP: round") {
                    rounds.push(integer(&scenario, "round").await);
                }
                if at == line(SOURCE, "// STEP: rounds-after") {
                    break;
                }
                assert!(rounds.len() < 3, "{context}: {rounds:?}");
            }
            assert_eq!(rounds, [Some(1), Some(2)], "{context}");
            scenario.shutdown().await;
        }
    }
}

/// `finish` from an async function whose await is pending runs it through
/// every poll to its return, and stops in its awaiter in the same task,
/// with what its last poll returned.
#[tokio::test]
async fn finish_returns_to_the_awaiter_in_the_same_task() {
    for fixture in BUILDS {
        for mode in MODES {
            let context = format!("{fixture} {mode:?}");
            let mut scenario = stopped_once(fixture, mode, "// STEP: inner").await;
            let task = stopped_task(&mut scenario).await;
            assert_eq!(
                step(&mut scenario, StepKind::Out).await,
                ("outer".to_owned(), line(SOURCE, "// AWAIT: outer")),
                "{context}"
            );
            assert_eq!(stopped_task(&mut scenario).await, task, "{context}");
            let returned = scenario
                .operation("variables", scenario.handle().variables())
                .await
                .variables
                .iter()
                .filter(|variable| variable.kind == VariableKind::Returned)
                .map(|variable| format!("{:?}", variable.state))
                .collect::<Vec<_>>();
            assert!(
                matches!(&returned[..], [poll] if poll.contains("Ready") && !poll.contains("Pending")),
                "{context}: {returned:#?}"
            );
            scenario.shutdown().await;
        }
    }
}

/// A step past the end of a task's own async function ends where the
/// function's future returns to tokio, saying the task finished: `next`
/// from its last line, and `finish`, which shows what it returned.
#[tokio::test]
async fn a_step_past_a_tasks_end_says_it_finished() {
    for fixture in BUILDS {
        for mode in MODES {
            for kind in [StepKind::OverSource, StepKind::Out] {
                let context = format!("{fixture} {mode:?} {kind:?}");
                let mut scenario = stopped_once(fixture, mode, "// STEP: task-last").await;
                let task = stopped_task(&mut scenario).await;
                let output = integer(&scenario, "got").await.expect("got")
                    + integer(&scenario, "more").await.expect("more");
                let mut reason = scenario.step_to_stop(kind).await;
                // `next` stops at the closing brace first.
                if reason == (StopReason::Step { kind }) {
                    assert_eq!(place(&scenario).await.0, "task", "{context}");
                    reason = scenario.step_to_stop(kind).await;
                }
                let StopReason::TaskEnded {
                    kind: ended,
                    task: finished,
                    ending: TaskEnding::Finished,
                } = reason
                else {
                    panic!("{context}: {reason:?}");
                };
                assert_eq!((ended, finished.number), (kind, task), "{context}");
                if kind == StepKind::Out {
                    let returned = scenario
                        .operation("variables", scenario.handle().variables())
                        .await
                        .variables
                        .iter()
                        .filter(|variable| variable.kind == VariableKind::Returned)
                        .map(|variable| format!("{:?}", variable.state))
                        .collect::<Vec<_>>();
                    assert!(
                        matches!(&returned[..], [poll] if poll.contains(&format!("summary: \"Ready({output})\""))),
                        "{context}: {output}: {returned:#?}"
                    );
                }
                scenario.shutdown().await;
            }
        }
    }
}

/// A breakpoint whose condition names a task by `$task` stops only in that
/// task, every time round its loop, on whichever worker runs it, while
/// every task's arrival counts as a hit: the line's executions.
#[tokio::test]
async fn a_task_condition_stops_only_in_its_task() {
    for fixture in BUILDS {
        for mode in MODES {
            let context = format!("{fixture} {mode:?}");
            let mut scenario = stopped_once(fixture, mode, "// STEP: task").await;
            let task = stopped_task(&mut scenario).await;
            let round = scenario.add_breakpoint_spec(at("// STEP: round")).await;
            scenario
                .operation(
                    "condition",
                    scenario.handle().set_breakpoint_condition(
                        round.id,
                        Some(
                            uscope::Condition::parse(&format!("$task == {task}"))
                                .expect("a condition"),
                        ),
                    ),
                )
                .await;
            let mut rounds = Vec::new();
            let mut reason = scenario.resume_to_stop().await;
            while matches!(reason, StopReason::Breakpoint { .. }) {
                assert_eq!(stopped_task(&mut scenario).await, task, "{context}");
                rounds.push(integer(&scenario, "round").await);
                assert!(rounds.len() <= 3, "{context}: {rounds:?}");
                reason = scenario.resume_to_stop().await;
            }
            assert!(
                matches!(reason, StopReason::Exited(_)),
                "{context}: {reason:?}"
            );
            assert_eq!(rounds, [Some(0), Some(1), Some(2)], "{context}");
            let hits = scenario
                .snapshot()
                .await
                .breakpoints
                .iter()
                .find(|breakpoint| breakpoint.id == round.id)
                .expect("the breakpoint")
                .hit_count;
            assert_eq!(hits, 9, "{context}: three tasks go round three times");
            scenario.shutdown().await;
        }
    }
}

/// A step of a task no thread runs, selected by its number, waits for the
/// task to resume, on whichever thread, and goes on from its await: `next`
/// from its innermost async function stops at that function's next line,
/// and `finish` from an outer function's frame returns to its awaiter.
#[tokio::test]
async fn a_step_of_a_suspended_task_waits_for_it_to_resume() {
    for fixture in BUILDS {
        for mode in MODES {
            for (kind, from, function, marker) in [
                (
                    StepKind::OverSource,
                    "inner",
                    "inner",
                    "// STEP: inner-after",
                ),
                (StepKind::Out, "outer", "task", "// STEP: task"),
            ] {
                let context = format!("{fixture} {mode:?} {kind:?}");
                // No task passes the gate before the first stops past it,
                // so the tasks no thread runs wait at the gate.
                let mut scenario = stopped_once(fixture, mode, "// STEP: inner-after").await;
                let page = scenario
                    .operation("tasks", scenario.handle().tasks(None, 16))
                    .await;
                let task = page
                    .tasks
                    .iter()
                    .find(|task| task.thread.is_none())
                    .unwrap_or_else(|| panic!("{context}: {:#?}", page.tasks))
                    .id;
                scenario
                    .operation("select task", scenario.handle().select_context(task))
                    .await;
                let frame = crate::stops::backtrace(&scenario)
                    .await
                    .frames
                    .iter()
                    .find(|frame| {
                        frame
                            .function
                            .as_ref()
                            .is_some_and(|function| *function.name == *from)
                    })
                    .unwrap_or_else(|| panic!("{context}: no frame of {from}"))
                    .id;
                scenario
                    .operation("select frame", scenario.handle().select_frame(frame))
                    .await;
                assert_eq!(
                    step(&mut scenario, kind).await,
                    (function.to_owned(), line(SOURCE, marker)),
                    "{context}"
                );
                assert_eq!(stopped_task(&mut scenario).await, task.number, "{context}");
                if kind == StepKind::OverSource {
                    // What the await keeps of the task's own number.
                    assert_eq!(
                        integer(&scenario, "before").await,
                        Some(i128::from(task.number) * 10),
                        "{context}"
                    );
                }
                scenario.shutdown().await;
            }
        }
    }
}
