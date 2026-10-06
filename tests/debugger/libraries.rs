//! Breakpoints in shared libraries, which load after the program starts.

use super::*;
use uscope::{Breakpoint, DebuggerEvent};

/// A function breakpoint kept while no loaded module defines the function.
pub async fn pending_function(scenario: &Scenario, name: &str) -> Breakpoint {
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

pub async fn breakpoint(scenario: &mut Scenario, id: uscope::BreakpointId) -> Breakpoint {
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
    let line = source_line("tests/fixtures/c/shared/library.c", "return *dso_pointer");
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
    assert_eq!(location_line(&location), Some(line));
    scenario.shutdown().await;
}

#[tokio::test]
async fn functions_without_debug_information_break_at_their_symbol() {
    let mut scenario = Scenario::launch("reexec");
    let puts = pending_function(&scenario, "puts").await;
    // An indirect function's symbol names its resolver, which runs once at
    // binding, not the function: it stays pending rather than mislead.
    let strstr = pending_function(&scenario, "strstr").await;
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            arguments: vec!["one".into()],
            ..LaunchOptions::default()
        })
        .await;
    let StopReason::Breakpoint { hits, .. } = &reason else {
        panic!("stopped for {reason:?}");
    };
    assert_eq!(hits[0].breakpoint, puts.id);
    assert_eq!(
        library_locations(&breakpoint(&mut scenario, puts.id).await),
        1
    );
    assert!(
        breakpoint(&mut scenario, strstr.id)
            .await
            .locations
            .is_empty()
    );
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let symbol = trace.frames[0].symbol.as_ref().expect("a symbol frame");
    // libc's `_IO_puts` names the same entry.
    assert!(
        symbol.name.ends_with("puts") && symbol.offset == 0,
        "{symbol:?}"
    );
    assert_eq!(
        trace.frames[1]
            .function
            .as_ref()
            .map(|function| &*function.name),
        Some("reexecuted")
    );
    scenario.shutdown().await;
}

/// The vDSO's symbols name code the program runs, so function breakpoints
/// resolve there, as gdb's do: libc binds `time` straight to the vDSO's, and
/// libc's own `clock_gettime` calls the vDSO's.
#[tokio::test]
async fn function_breakpoints_resolve_in_the_vdso() {
    const FIXTURE: &str = "vdso-gcc-o0";
    let mut scenario = Scenario::launch(FIXTURE);
    let time = pending_function(&scenario, "time").await;
    let clock = pending_function(&scenario, "clock_gettime").await;
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            arguments: vec!["calls".into()],
            ..LaunchOptions::default()
        })
        .await;
    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    let vdso = support::vdso_module(&modules).module.id;
    let libc = modules
        .modules
        .iter()
        .find(|record| {
            record
                .path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("libc.so"))
        })
        .expect("libc is loaded")
        .module
        .id;
    let libraries = |breakpoint: &Breakpoint| {
        breakpoint
            .locations
            .iter()
            .map(|location| location.library)
            .collect::<BTreeSet<_>>()
    };
    // libc's own `time` names its resolver, which never runs as `time`.
    let time = breakpoint(&mut scenario, time.id).await;
    assert_eq!(libraries(&time), BTreeSet::from([Some(vdso)]), "{time:#?}");
    let clock = breakpoint(&mut scenario, clock.id).await;
    assert_eq!(
        libraries(&clock),
        BTreeSet::from([Some(libc), Some(vdso)]),
        "{clock:#?}"
    );

    // Each stop is at the start of a function, in the module the
    // breakpoint's location belongs to, below the program's caller.
    let expected = [
        (time.id, vdso, "vdso_time"),
        (clock.id, libc, "vdso_clock"),
        (clock.id, vdso, "vdso_clock"),
    ];
    let mut reason = reason;
    for (index, (id, module, caller)) in expected.into_iter().enumerate() {
        assert!(
            matches!(&reason, StopReason::Breakpoint { hits, .. } if hits[0].breakpoint == id),
            "stop {index}: {reason:?}"
        );
        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        let frames = frame_modules(&trace, &modules);
        assert_eq!(
            trace.frames[0].module,
            Some(module),
            "stop {index}: {frames:#?}"
        );
        let symbol = trace.frames[0].symbol.as_ref().expect("a symbol");
        assert_eq!(symbol.offset, 0, "stop {index}: {symbol:?}");
        assert!(
            position_of(&frames, FIXTURE, caller) <= 2,
            "stop {index}: {frames:#?}"
        );
        reason = scenario.resume_to_stop().await;
    }
    assert_eq!(reason, StopReason::Exited(ExitStatus::Code(0)));
    scenario.shutdown().await;
}

/// A process may move its vDSO, as a checkpoint restore does, or unmap it.
/// The vDSO's module is read again where it moved to, and goes with it.
#[tokio::test]
async fn the_vdso_module_follows_the_vdso_as_it_moves_and_goes() {
    let mut scenario = Scenario::launch("vdso-gcc-o0");
    scenario.add_breakpoint("vdso_moved").await;
    scenario.add_breakpoint("vdso_unmapped").await;
    let mut events = scenario.handle().subscribe();
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            arguments: vec!["move".into()],
            ..LaunchOptions::default()
        })
        .await;
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{reason:?}"
    );
    let InferiorState::Stopped { process_id, .. } = scenario.snapshot().await.inferior else {
        panic!("the program is stopped");
    };
    let mut vdso_events = || {
        std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| match event {
                DebuggerEvent::ModuleLoaded { module, .. } => Some((true, module)),
                DebuggerEvent::ModuleUnloaded { module, .. } => Some((false, module)),
                _ => None,
            })
            .filter(|(_, module)| module.path.as_os_str() == support::VDSO)
            .map(|(loaded, module)| (loaded, module.module.load_bias))
            .collect::<Vec<_>>()
    };
    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    let moved = support::vdso_mapping(process_id).start;
    assert_eq!(support::vdso_module(&modules).module.load_bias, moved);
    let target = scenario
        .operation("address", scenario.handle().variable("address"))
        .await;
    assert!(
        matches!(
            available_value(&target.state),
            uscope::VariableValue::Address(value) if value.address.get() == moved
        ),
        "{target:?}"
    );
    // The vDSO the program started with was registered, then replaced.
    let changes = vdso_events();
    let [(true, first), (false, unloaded), (true, loaded)] = changes[..] else {
        panic!("{changes:x?}");
    };
    assert!(
        first == unloaded && first != moved && loaded == moved,
        "{changes:x?}"
    );

    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    assert!(
        modules
            .modules
            .iter()
            .all(|record| record.path.as_os_str() != support::VDSO),
        "{modules:#?}"
    );
    assert_eq!(vdso_events(), [(false, moved)]);
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}
