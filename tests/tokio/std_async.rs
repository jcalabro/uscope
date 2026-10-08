//! Async functions under a small executor of the program's own, with no
//! runtime crate: names, breakpoints, steps within a poll, and values.

use std::process::Stdio;

use uscope::{BreakpointSpec, LaunchOptions, LineNumber, StopReason};

use crate::stops::{backtrace, frames_to, integer, line, place};
use crate::support::Scenario;

const BUILDS: [&str; 2] = ["tokio-std-async-o0", "tokio-std-async-o3"];
const SOURCE: &str = "std-async/src/main.rs";

/// A fixture launched to its first stop at the breakpoint `spec`.
async fn stopped_at(fixture: &str, spec: BreakpointSpec) -> Scenario {
    let mut scenario = Scenario::launch(fixture);
    scenario.add_breakpoint_spec(spec).await;
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            stdout: Some(Stdio::null()),
            ..LaunchOptions::default()
        })
        .await;
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{fixture}: {reason:?}"
    );
    scenario
}

fn at(marker: &str) -> BreakpointSpec {
    BreakpointSpec::Source {
        path: SOURCE.into(),
        line: LineNumber::new(line(SOURCE, marker)).expect("one-based"),
    }
}

/// Each async body is named for the function its programmer wrote, and an
/// async block for its function and number, in both builds: optimization
/// inlines the bodies into one another, and their frames stay.
#[tokio::test]
async fn async_bodies_are_named_for_their_functions() {
    for fixture in BUILDS {
        let scenario = stopped_at(fixture, at("// STEP: leaf-after")).await;
        let trace = backtrace(&scenario).await;
        assert_eq!(
            frames_to(&trace, "run"),
            [
                ("leaf".to_owned(), line(SOURCE, "// STEP: leaf-after")),
                ("middle".to_owned(), line(SOURCE, "// AWAIT: middle")),
                (
                    "main::{async block#0}".to_owned(),
                    line(SOURCE, "// STEP: task-a")
                ),
                ("run".to_owned(), line(SOURCE, "task.as_mut().poll")),
            ],
            "{fixture}: {trace:#?}"
        );
        scenario.shutdown().await;
    }
}

/// `break leaf` binds the body of the async function, past its dispatch on
/// the state, never the function that only builds its future: it stops at
/// the body's first statement, once per call.
#[tokio::test]
async fn a_function_breakpoint_binds_an_async_body() {
    for fixture in BUILDS {
        let mut scenario = Scenario::launch(fixture);
        let breakpoint = scenario.add_breakpoint("leaf").await;
        assert_eq!(breakpoint.locations.len(), 1, "{fixture}: {breakpoint:#?}");
        let mut ids = Vec::new();
        let mut reason = scenario
            .run_with_to_stop(LaunchOptions {
                stdout: Some(Stdio::null()),
                ..LaunchOptions::default()
            })
            .await;
        while matches!(reason, StopReason::Breakpoint { .. }) {
            assert_eq!(
                place(&scenario).await,
                ("leaf".to_owned(), line(SOURCE, "// STEP: leaf")),
                "{fixture}"
            );
            ids.push(integer(&scenario, "id").await);
            assert!(ids.len() <= 2, "{fixture}: {ids:?}");
            reason = scenario.resume_to_stop().await;
        }
        assert!(
            matches!(reason, StopReason::Exited { .. }),
            "{fixture}: {reason:?}"
        );
        // Optimization may leave the argument without a location, but
        // never with another value.
        assert_eq!(ids.len(), 2, "{fixture}");
        for (id, expected) in ids.iter().zip([3, 4]) {
            assert!(
                *id == Some(expected) || (fixture.ends_with("o3") && id.is_none()),
                "{fixture}: {ids:?}"
            );
        }
        scenario.shutdown().await;
    }
}

/// A breakpoint on an await's line stops when execution arrives at the
/// await, once per arrival, never again as the future resumes there after
/// it was pending; and it binds no copy of the line in the code that drops
/// the future.
#[tokio::test]
async fn await_lines_stop_on_arrival_only() {
    for fixture in BUILDS {
        for (marker, arrivals) in [("// AWAIT: leaf", 2), ("// AWAIT: walk", 3)] {
            let mut scenario = Scenario::launch(fixture);
            let breakpoint = scenario.add_breakpoint_spec(at(marker)).await;
            assert_eq!(
                breakpoint.locations.len(),
                1,
                "{fixture} {marker}: {breakpoint:#?}"
            );
            let mut stops = 0;
            let mut reason = scenario
                .run_with_to_stop(LaunchOptions {
                    stdout: Some(Stdio::null()),
                    ..LaunchOptions::default()
                })
                .await;
            while matches!(reason, StopReason::Breakpoint { .. }) {
                stops += 1;
                assert!(stops <= arrivals, "{fixture} {marker}");
                reason = scenario.resume_to_stop().await;
            }
            assert!(
                matches!(reason, StopReason::Exited { .. }),
                "{fixture} {marker}: {reason:?}"
            );
            assert_eq!(stops, arrivals, "{fixture} {marker}");
            scenario.shutdown().await;
        }
    }
}
