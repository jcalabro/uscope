//! Breakpoints in shared libraries, which load after the program starts.

use super::*;
use uscope::{Breakpoint, DebuggerEvent};

/// A function breakpoint kept while no loaded module defines the function.
async fn pending_function(scenario: &Scenario, name: &str) -> Breakpoint {
    scenario
        .operation(
            "add pending breakpoint",
            scenario.handle().add_breakpoint_with(
                BreakpointSpec::Function(name.to_owned()),
                uscope::BreakpointOptions {
                    pending: true,
                    ..uscope::BreakpointOptions::default()
                },
            ),
        )
        .await
}

fn library_locations(breakpoint: &Breakpoint) -> usize {
    breakpoint
        .locations
        .iter()
        .filter(|location| location.library.is_some())
        .count()
}

async fn breakpoint(scenario: &mut Scenario, id: uscope::BreakpointId) -> Breakpoint {
    scenario
        .snapshot()
        .await
        .breakpoints
        .iter()
        .find(|breakpoint| breakpoint.id == id)
        .expect("breakpoint")
        .clone()
}

#[tokio::test]
async fn breakpoints_in_a_library_the_program_links_resolve_before_it_runs() {
    let mut scenario = Scenario::launch("module-frames-gcc-o0");
    // Without the pending option, a function nothing defines is refused;
    // the program only declares it.
    let refused = scenario
        .attempt(
            "add missing breakpoint",
            scenario
                .handle()
                .add_breakpoint(BreakpointSpec::Function("dso_apply".to_owned())),
        )
        .await;
    assert!(matches!(refused, Err(uscope::Error::FunctionNotFound(_))));
    let pending = pending_function(&scenario, "dso_apply").await;
    assert!(
        pending.locations.is_empty(),
        "nothing defines it before launch"
    );
    let reason = scenario.run_to_stop().await;
    let StopReason::Breakpoint { hits, .. } = &reason else {
        panic!("stopped for {reason:?}");
    };
    assert_eq!(hits[0].breakpoint, pending.id);
    let resolved = breakpoint(&mut scenario, pending.id).await;
    assert_eq!(library_locations(&resolved), 1);
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    assert_eq!(
        trace.frames[0]
            .function
            .as_ref()
            .map(|function| &*function.name),
        Some("dso_apply")
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn breakpoints_follow_a_library_through_dlopen_and_dlclose() {
    let mut scenario = Scenario::launch("globals-shared");
    let touch = pending_function(&scenario, "dso_touch").await;
    scenario.add_breakpoint("after_unload").await;
    let mut events = scenario.handle().subscribe();
    // The library is opened, used, closed, and opened again: its breakpoint
    // stops in each use, and is pending between them.
    for (round, expected) in [(1, "dso_touch"), (1, "after_unload"), (2, "dso_touch")] {
        let reason = scenario.resume_or_run().await;
        let StopReason::Breakpoint { hits, .. } = &reason else {
            panic!("round {round} stopped for {reason:?}");
        };
        let stopped = scenario
            .snapshot()
            .await
            .breakpoints
            .iter()
            .find(|breakpoint| breakpoint.id == hits[0].breakpoint)
            .expect("hit breakpoint")
            .spec
            .to_string();
        assert_eq!(stopped, expected, "round {round}");
        let touch_now = breakpoint(&mut scenario, touch.id).await;
        let expected_locations = usize::from(expected == "dso_touch");
        assert_eq!(
            library_locations(&touch_now),
            expected_locations,
            "round {round}"
        );
    }
    let changes = std::iter::from_fn(|| events.try_recv().ok())
        .filter(|event| matches!(event, DebuggerEvent::BreakpointsChanged { .. }))
        .count();
    assert!(
        changes >= 3,
        "loads and unloads change the breakpoint: {changes}"
    );
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    // A new process starts with the library unloaded again.
    assert!(
        breakpoint(&mut scenario, touch.id)
            .await
            .locations
            .is_empty()
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn source_breakpoints_resolve_in_library_source_once_it_loads() {
    let mut scenario = Scenario::launch("globals-shared");
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/c/shared/library.c");
    let line = std::fs::read_to_string(&path)
        .expect("source")
        .lines()
        .position(|line| line.contains("return *dso_pointer"))
        .expect("line") as u64
        + 1;
    let breakpoint = scenario
        .operation(
            "add pending source breakpoint",
            scenario.handle().add_breakpoint_with(
                BreakpointSpec::Source {
                    path: path.clone(),
                    line: uscope::LineNumber::new(line).expect("line"),
                },
                uscope::BreakpointOptions {
                    pending: true,
                    ..uscope::BreakpointOptions::default()
                },
            ),
        )
        .await;
    assert!(breakpoint.locations.is_empty());
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { hits, .. } if hits[0].breakpoint == breakpoint.id
    ));
    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;
    assert_eq!(
        location
            .image
            .source
            .as_ref()
            .map(|source| source.line.get()),
        Some(line)
    );
    scenario.shutdown().await;
}
