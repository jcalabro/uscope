//! Stepping in stops only in code the program wrote. It passes over the
//! runtime's own machinery, the wrappers the compiler generates, and the
//! stack check that grows a goroutine's stack, staying in its goroutine.

use std::process::Stdio;

use uscope::{BreakpointSpec, LaunchOptions, LineNumber, StepKind, StopReason};

use crate::stops::{integer, place};
use crate::support::{self, Scenario};

const SOURCE: &str = "tests/fixtures/go/steps/main.go";

fn line(marker: &str) -> u64 {
    support::source_line(SOURCE, marker)
}

/// A fixture stopped at the first line of `main.run`'s walk, with no
/// breakpoint left, and the goroutine it runs in.
async fn at_the_walk(fixture: &str) -> (Scenario, i128) {
    let mut scenario = crate::invariants::checked(fixture);
    let breakpoint = scenario
        .add_breakpoint_spec(BreakpointSpec::Source {
            path: "steps/main.go".into(),
            line: LineNumber::new(line("// STEP: map")).expect("one-based"),
        })
        .await;
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
    scenario.remove_breakpoint(breakpoint.id).await;
    let task = integer(&scenario, "$task").await.expect("a task");
    (scenario, task)
}

#[tokio::test]
async fn stepping_in_stops_only_in_code_the_program_wrote() {
    let fixture = "steps-go-o0";
    let (mut scenario, task) = at_the_walk(fixture).await;
    // Each step, and the function and line it ends at.
    let walk = [
        // Past the runtime's map assignment.
        (StepKind::IntoSource, "main.run", "// STEP: list"),
        (StepKind::IntoSource, "main.run", "// STEP: append"),
        // Past the runtime's growing of the slice.
        (StepKind::IntoSource, "main.run", "// STEP: shape"),
        (StepKind::IntoSource, "main.run", "// STEP: interface"),
        // Through the wrapper the interface's method table calls.
        (StepKind::IntoSource, "main.square.area", "// STEP: area"),
        (StepKind::Out, "main.run", "// STEP: interface"),
        // A `go` statement starts its goroutine in the runtime.
        (StepKind::IntoSource, "main.run", "// STEP: go"),
        (StepKind::IntoSource, "main.run", "// STEP: grow"),
        // Into a function whose stack check grows the stack first.
        (StepKind::IntoSource, "main.fresh", "// STEP: fresh"),
        (StepKind::Out, "main.run", "// STEP: grow"),
        (StepKind::IntoSource, "main.run", "// STEP: after"),
    ];
    for (kind, function, marker) in walk {
        let reason = scenario.step_to_stop(kind).await;
        assert_eq!(reason, StopReason::Step { kind }, "{fixture}: to {marker}");
        assert_eq!(
            place(&scenario).await,
            (function.to_owned(), line(marker)),
            "{fixture}: {kind:?} to {marker}"
        );
        assert_eq!(
            integer(&scenario, "$task").await,
            Some(task),
            "{fixture}: {marker}"
        );
    }
    scenario.shutdown().await;
}

#[tokio::test]
async fn optimized_steps_stop_only_in_code_the_program_wrote() {
    let fixture = "steps-go-o2";
    let (mut scenario, task) = at_the_walk(fixture).await;
    let after = line("// STEP: after");
    let mut visited = Vec::new();
    loop {
        assert!(visited.len() < 40, "{fixture}: {visited:?}");
        let reason = scenario.step_to_stop(StepKind::IntoSource).await;
        assert_eq!(
            reason,
            StopReason::Step {
                kind: StepKind::IntoSource
            },
            "{fixture}: after {visited:?}"
        );
        let (function, line) = place(&scenario).await;
        assert!(
            ["main.run", "main.square.area", "main.fresh"].contains(&function.as_str()),
            "{fixture}: {function}:{line} after {visited:?}"
        );
        assert_eq!(integer(&scenario, "$task").await, Some(task), "{fixture}");
        visited.push((function, line));
        if visited.last().is_some_and(|(_, line)| *line == after) {
            break;
        }
    }
    for function in ["main.square.area", "main.fresh"] {
        assert!(
            visited.iter().any(|(name, _)| name == function),
            "{fixture}: {visited:?}"
        );
    }
    scenario.shutdown().await;
}
