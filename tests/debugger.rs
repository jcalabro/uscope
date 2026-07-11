mod support;

use uscope::{BreakpointLocation, Error, ExitStatus, InferiorState, StopReason, UnwindTermination};

use support::Scenario;

#[tokio::test]
async fn breakpoint_memory_and_event_state_follow_one_consistent_scenario() {
    let mut scenario = Scenario::new("breakpoint lifecycle", Scenario::fixture("basic"));

    let breakpoint = scenario.add_breakpoint("breakpoint_target").await;
    let first = scenario.run_to_stop().await;

    let first_address = match first {
        StopReason::Breakpoint { address } => address,
        other => panic!("expected breakpoint, got {other:?}"),
    };

    let location = scenario
        .operation("current location", scenario.handle().current_location())
        .await;

    assert_eq!(location.address, first_address);
    assert_eq!(
        location
            .image
            .function
            .as_ref()
            .map(|function| function.name.as_ref()),
        Some("breakpoint_target")
    );

    let source = location.image.source.as_ref().expect("source location");
    let source_file = scenario
        .handle()
        .module_image()
        .source_file(source.file)
        .expect("source file");

    assert!(source_file.path.ends_with("basic.c"));
    assert!(source.line.get() > 0);

    let BreakpointLocation::Image(image_address) = breakpoint else {
        panic!("function breakpoint was not image-based")
    };

    assert_ne!(
        first_address.get(),
        image_address.get(),
        "PIE was not relocated"
    );

    let snapshot = scenario.snapshot().await;

    assert_eq!(snapshot.revision, scenario.last_revision());
    assert!(matches!(
        snapshot.inferior,
        InferiorState::Stopped { reason, .. } if reason == first
    ));
    assert_eq!(snapshot.breakpoints.as_ref(), &[breakpoint]);

    let duplicate = scenario.add_breakpoint("breakpoint_target").await;

    assert_eq!(duplicate, breakpoint);
    assert_eq!(
        scenario.snapshot().await.breakpoints.as_ref(),
        &[breakpoint]
    );

    let main_breakpoint = scenario.add_breakpoint("main").await;

    assert_eq!(
        scenario.snapshot().await.breakpoints.as_ref(),
        &[breakpoint, main_breakpoint]
    );

    let value_address = scenario
        .operation(
            "resolve uscope_value",
            scenario.handle().runtime_address("uscope_value"),
        )
        .await;

    assert_eq!(
        scenario
            .operation(
                "read uscope_value",
                scenario.handle().read_word(value_address)
            )
            .await,
        0x1122_3344_5566_7788
    );

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint {
            address: first_address
        }
    );
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );

    scenario.shutdown().await;
}

#[tokio::test]
async fn shutdown_reaps_running_and_stopped_inferiors() {
    let mut running = Scenario::new("shutdown running", Scenario::fixture("spin"));

    let run = running.start_running().await;

    assert!(matches!(
        running.snapshot().await.inferior,
        InferiorState::Running { .. }
    ));

    let status = running.shutdown().await.expect("inferior exit event");

    assert!(matches!(
        status,
        ExitStatus::Terminated(exception) if exception.code == 9
    ));
    assert!(matches!(
        run.await.expect("run task"),
        Err(Error::RequestCancelled)
    ));

    let mut stopped = Scenario::new("shutdown stopped", Scenario::fixture("basic"));

    stopped.add_breakpoint("breakpoint_target").await;

    assert!(matches!(
        stopped.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    stopped.shutdown().await;
}

#[tokio::test]
async fn dwarf_cfi_unwinds_the_compiler_and_linker_matrix() {
    for fixture in ["unwind-o0", "unwind-o2", "unwind-nopie", "unwind-clang-o2"] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));

        scenario.add_breakpoint("deepest").await;

        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;

        let names: Vec<_> = trace
            .frames
            .iter()
            .filter_map(|frame| frame.function.as_ref())
            .map(|function| function.name.as_ref())
            .collect();

        assert!(
            names.starts_with(&["deepest", "middle", "outer", "main"]),
            "unexpected {fixture} backtrace: {trace:?}"
        );
        assert!(
            trace.frames.len() >= 4,
            "backtrace was truncated: {trace:?}"
        );
        assert!(matches!(
            trace.termination,
            UnwindTermination::ModuleNotFound { .. }
                | UnwindTermination::NoUnwindInfo { .. }
                | UnwindTermination::Complete
        ));

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn repeated_debug_sessions_leave_no_inferiors_behind() {
    for iteration in 0..8 {
        let mut scenario = Scenario::new(
            format!("repeated session {iteration}"),
            Scenario::fixture("basic"),
        );

        scenario.add_breakpoint("breakpoint_target").await;

        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        scenario.shutdown().await;
    }
}
