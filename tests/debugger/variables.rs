//! Parameters and locals across languages, scopes, and threads.

use super::*;

#[tokio::test]
async fn stack_scalar_variables_are_read_through_the_public_scenario_path() {
    for fixture in ["variables-gcc-o0", "variables-clang-o0"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_source_breakpoint("variables.c", 108).await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let before = scenario.snapshot().await;
        let mut inspection_events = scenario.handle().subscribe();

        let snapshot = scenario
            .operation("inspect variables", scenario.handle().variables())
            .await;
        let names = snapshot
            .variables
            .iter()
            .map(|variable| variable.name.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "boolean",
                "character",
                "signed_character",
                "unsigned_character",
                "signed_short",
                "unsigned_short",
                "signed_int",
                "unsigned_int",
                "signed_long",
                "unsigned_long",
                "signed_long_long",
                "unsigned_long_long",
                "single",
                "double_precision",
                "extended",
            ]
        );
        let expected = [
            ScalarValue::Boolean(true),
            ScalarValue::Signed(65),
            ScalarValue::Signed(-12),
            ScalarValue::Unsigned(250),
            ScalarValue::Signed(-1234),
            ScalarValue::Unsigned(54_321),
            ScalarValue::Signed(-1_234_567),
            ScalarValue::Unsigned(3_456_789_012),
            ScalarValue::Signed(-123_456_789),
            ScalarValue::Unsigned(123_456_789),
            ScalarValue::Signed(-1_234_567_890_123),
            ScalarValue::Unsigned(12_345_678_901_234),
            ScalarValue::Floating(uscope::FloatValue::Binary32(1.25_f32.to_bits())),
            ScalarValue::Floating(uscope::FloatValue::Binary64((-2.5_f64).to_bits())),
            ScalarValue::Floating(uscope::FloatValue::X87Extended {
                significand: 0xc800_0000_0000_0000,
                sign_exponent: 0x4000,
            }),
        ];
        let expected_sizes = [1, 1, 1, 1, 2, 2, 4, 4, 8, 8, 8, 8, 4, 8, 16];
        for ((variable, expected), expected_size) in
            snapshot.variables.iter().zip(expected).zip(expected_sizes)
        {
            assert_variable_value(variable, expected);
            assert_eq!(
                variable
                    .type_info
                    .as_ref()
                    .expect("available scalar type")
                    .byte_size,
                Some(expected_size)
            );
            let VariableState::Available { source, raw, .. } = &variable.state else {
                unreachable!("value assertion checked availability")
            };
            assert!(
                matches!(source, uscope::VariableValueSource::Memory(address) if address.get() != 0)
            );
            assert_eq!(
                raw.as_ref().expect("available scalar bytes").len(),
                usize::try_from(expected_size).unwrap()
            );
        }
        assert_eq!(
            scenario
                .operation(
                    "inspect signed_int",
                    scenario.handle().variable("signed_int")
                )
                .await,
            snapshot.variables[6]
        );
        let after = scenario.snapshot().await;
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.inferior, before.inferior);
        assert!(matches!(
            inspection_events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn stack_scalar_parameters_are_read_through_the_public_scenario_path() {
    for fixture in [
        "variables-parameters-gcc-o0",
        "variables-parameters-clang-o0",
    ] {
        let mut scenario = Scenario::launch(fixture);
        scenario
            .add_source_breakpoint("variables-parameters.c", 22)
            .await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let snapshot = scenario
            .operation("inspect parameters", scenario.handle().variables())
            .await;
        assert_all_parameter_values(&snapshot, fixture);
        assert_eq!(
            scenario
                .operation(
                    "inspect signed parameter",
                    scenario.handle().variable("signed_int")
                )
                .await,
            snapshot.variables[6]
        );
        assert!(matches!(
            scenario.handle().variable("missing").await,
            Err(Error::VariableNotFound(name)) if name == "missing"
        ));
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn optimized_physical_parameters_materialize_supported_dwarf_locations() {
    for fixture in [
        "variables-parameters-gcc-o2",
        "variables-parameters-clang-o2",
    ] {
        let mut scenario = Scenario::launch(fixture);
        scenario
            .add_source_breakpoint("variables-parameters.c", 22)
            .await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let snapshot = scenario
            .operation(
                "inspect optimized parameters",
                scenario.handle().variables(),
            )
            .await;
        assert_eq!(snapshot.frame, uscope::PresentedFrame::Physical);
        assert_parameter_catalog(&snapshot, fixture);
        assert_optimized_parameter_values(&snapshot, fixture);
        assert_eq!(
            scenario
                .operation(
                    "inspect optimized register parameter",
                    scenario.handle().variable("boolean")
                )
                .await,
            snapshot.variables[0]
        );
        assert_eq!(
            scenario
                .operation(
                    "inspect available optimized parameter",
                    scenario.handle().variable("signed_int")
                )
                .await,
            snapshot.variables[6]
        );
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn physical_function_breakpoints_stop_after_the_prologue_with_readable_parameters() {
    for case in entry_boundary_cases() {
        let mut scenario = Scenario::new(
            format!("function entry boundary {}", case.fixture),
            Scenario::fixture(case.fixture),
        );
        let expected = expected_physical_entry(&scenario, &case);
        let breakpoint = scenario.add_breakpoint(case.function).await;
        assert_eq!(
            single_image_breakpoint_address(&breakpoint),
            expected,
            "{} function breakpoint did not use its recommended physical entry",
            case.fixture
        );

        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        assert_entry_stop(&scenario, &case, expected).await;
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn static_locals_resolve_relocated_and_indexed_addresses() {
    for fixture in [
        "variables-static-gcc-o2",
        "variables-static-clang-o2",
        "variables-static-gcc-nopie",
    ] {
        let mut scenario = Scenario::launch(fixture);
        scenario
            .add_source_breakpoint("variables-static.c", 7)
            .await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let snapshot = scenario
            .operation("inspect static local", scenario.handle().variables())
            .await;
        assert_eq!(snapshot.variables.len(), 1, "{fixture}: {snapshot:?}");
        assert_eq!(snapshot.variables[0].name.as_ref(), "static_value");
        assert_variable_value(&snapshot.variables[0], ScalarValue::Signed(73));
        assert_memory_source(&snapshot.variables[0], fixture);

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn cpp_and_rust_stack_scalars_use_the_public_variable_path() {
    for (fixture, source, line) in [
        ("variables-cpp-gcc-o0", "variables.cpp", 27),
        ("variables-cpp-clang-o0", "variables.cpp", 27),
        ("variables-rust-o0", "variables.rs", 33),
    ] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_source_breakpoint(source, line).await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let snapshot = scenario
            .operation("inspect language scalars", scenario.handle().variables())
            .await;
        assert_language_scalar_values(&snapshot, fixture);
        assert_eq!(
            scenario
                .operation(
                    "inspect language parameter",
                    scenario.handle().variable("signed_value")
                )
                .await,
            snapshot.variables[1]
        );
        assert_eq!(
            scenario
                .operation(
                    "inspect language local",
                    scenario.handle().variable("local_double")
                )
                .await,
            snapshot.variables[9]
        );
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn optimized_cpp_and_rust_scalars_materialize_supported_locations() {
    for (fixture, source, line) in [
        ("variables-cpp-gcc-o2", "variables.cpp", 27),
        ("variables-cpp-clang-o2", "variables.cpp", 27),
        ("variables-rust-o2", "variables.rs", 34),
    ] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_source_breakpoint(source, line).await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let snapshot = scenario
            .operation(
                "inspect optimized language scalars",
                scenario.handle().variables(),
            )
            .await;
        assert_language_scalar_catalog(&snapshot, fixture);
        assert_optimized_language_scalar_values(&snapshot, fixture);
        assert_eq!(
            scenario
                .operation(
                    "inspect optimized language parameter",
                    scenario.handle().variable("signed_value")
                )
                .await,
            snapshot.variables[1]
        );
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn go_scalars_are_printable_at_a_user_breakpoint_without_stepping() {
    let fixture = "variables-go-o0";
    let mut scenario = Scenario::launch(fixture);
    scenario.add_source_breakpoint("main.go", 49).await;
    run_go_to_breakpoint(&mut scenario, fixture).await;

    let state = scenario.snapshot().await;
    assert!(
        state
            .threads
            .iter()
            .all(|thread| matches!(thread.state, ThreadState::Stopped { .. }))
    );

    let snapshot = scenario
        .operation("inspect Go scalars", scenario.handle().variables())
        .await;
    for (name, expected) in [
        ("flag", ScalarValue::Boolean(true)),
        ("signedValue", ScalarValue::Signed(-42)),
        ("unsignedValue", ScalarValue::Unsigned(42)),
        (
            "single",
            ScalarValue::Floating(uscope::FloatValue::Binary32(1.25_f32.to_bits())),
        ),
        (
            "doublePrecision",
            ScalarValue::Floating(uscope::FloatValue::Binary64((-2.5_f64).to_bits())),
        ),
        ("localFlag", ScalarValue::Boolean(false)),
        ("localSigned", ScalarValue::Signed(-41)),
        ("localUnsigned", ScalarValue::Unsigned(44)),
        (
            "localSingle",
            ScalarValue::Floating(uscope::FloatValue::Binary32(1.75_f32.to_bits())),
        ),
        (
            "localDouble",
            ScalarValue::Floating(uscope::FloatValue::Binary64((-2.75_f64).to_bits())),
        ),
    ] {
        let variable = snapshot
            .variables
            .iter()
            .find(|variable| variable.name.as_ref() == name)
            .unwrap_or_else(|| panic!("{fixture} did not expose {name}: {snapshot:?}"));
        assert_variable_value(variable, expected);
    }

    assert_go_pointer_values(&scenario, fixture).await;

    resume_go_to_exit(&mut scenario, fixture).await;
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

#[tokio::test]
async fn go_variable_lookup_respects_nested_lexical_shadowing() {
    let fixture = "variables-go-o0";
    let mut scenario = Scenario::new("Go lexical shadowing", Scenario::fixture(fixture));
    scenario.add_source_breakpoint("main.go", 77).await;
    run_go_to_breakpoint(&mut scenario, fixture).await;

    let innermost = scenario
        .operation(
            "Go innermost shadow",
            scenario.handle().variable("shadowed"),
        )
        .await;
    assert_variable_value(&innermost, ScalarValue::Signed(200));
    let snapshot = scenario
        .operation("Go shadow catalog", scenario.handle().variables())
        .await;
    let shadows = snapshot
        .variables
        .iter()
        .filter(|variable| variable.name.as_ref() == "shadowed")
        .collect::<Vec<_>>();
    assert_eq!(shadows.len(), 2, "{snapshot:?}");
    assert_variable_value(shadows[0], ScalarValue::Signed(100));
    assert_variable_value(shadows[1], ScalarValue::Signed(200));

    resume_go_to_exit(&mut scenario, fixture).await;
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

#[tokio::test]
async fn zig_scalars_cover_pie_nonpie_and_optimized_partial_locations() {
    for fixture in ["variables-zig-o0", "variables-zig-nopie"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_source_breakpoint("variables.zig", 37).await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let snapshot = scenario
            .operation("inspect Zig scalars", scenario.handle().variables())
            .await;
        assert_language_scalar_values(&snapshot, fixture);
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }

    let fixture = "variables-zig-o2";
    let mut scenario = Scenario::launch(fixture);
    scenario.add_breakpoint("inspectScalars").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let snapshot = scenario
        .operation(
            "inspect optimized Zig scalars",
            scenario.handle().variables(),
        )
        .await;
    for (name, expected) in [
        ("flag", ScalarValue::Boolean(true)),
        ("signed_value", ScalarValue::Signed(-42)),
        ("unsigned_value", ScalarValue::Unsigned(42)),
        (
            "single",
            ScalarValue::Floating(uscope::FloatValue::Binary32(1.25_f32.to_bits())),
        ),
        (
            "double_precision",
            ScalarValue::Floating(uscope::FloatValue::Binary64((-2.5_f64).to_bits())),
        ),
        ("local_signed", ScalarValue::Signed(-41)),
    ] {
        let variable = snapshot
            .variables
            .iter()
            .find(|variable| variable.name.as_ref() == name)
            .unwrap_or_else(|| panic!("{fixture} did not expose {name}: {snapshot:?}"));
        assert_variable_value(variable, expected);
    }
    // The remaining locals have no location at the stop.
    let unavailable = snapshot
        .variables
        .iter()
        .filter(|variable| {
            matches!(
                variable.state,
                VariableState::Unavailable(
                    uscope::VariableUnavailableReason::UnavailableAtInstruction
                )
            )
        })
        .map(|variable| variable.name.as_ref())
        .collect::<Vec<_>>();
    assert_eq!(
        unavailable,
        ["local_unsigned", "local_single", "local_double"],
        "{snapshot:?}"
    );
    assert_eq!(snapshot.variables.len(), 9, "{snapshot:?}");
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

#[tokio::test]
async fn zig_variable_lookup_tracks_nested_lexical_scope() {
    let fixture = "variables-zig-o0";
    let mut scenario = Scenario::new("Zig lexical scope", Scenario::fixture(fixture));
    scenario.add_source_breakpoint("variables.zig", 50).await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    let snapshot = scenario
        .operation("inspect Zig nested scope", scenario.handle().variables())
        .await;
    for (name, expected) in [
        ("value", ScalarValue::Signed(-42)),
        ("outer_value", ScalarValue::Signed(-41)),
        ("nested_value", ScalarValue::Signed(-40)),
    ] {
        let variable = snapshot
            .variables
            .iter()
            .find(|variable| variable.name.as_ref() == name)
            .unwrap_or_else(|| panic!("{fixture} did not expose {name}: {snapshot:?}"));
        assert_variable_value(variable, expected);
    }

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

#[tokio::test]
async fn zig_native_threads_are_all_stopped_selectable_and_variable_aware() {
    let fixture = "variables-threads-zig";
    let mut scenario = Scenario::launch(fixture);
    scenario
        .add_source_breakpoint("variables-threads.zig", 8)
        .await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    let snapshot = scenario.snapshot().await;
    assert_eq!(snapshot.threads.len(), 3, "{snapshot:?}");
    assert!(
        snapshot
            .threads
            .iter()
            .all(|thread| matches!(thread.state, ThreadState::Stopped { .. }))
    );
    // Each worker's value is a parameter of its worker frame, which a
    // worker still spinning on the release flag shows below an inline frame.
    let stop = snapshot.stop_id.expect("stopped");
    let mut values = BTreeSet::new();
    for thread in snapshot.threads.iter() {
        scenario
            .operation(
                "select Zig thread",
                scenario.handle().select_thread(thread.id),
            )
            .await;
        let trace = scenario
            .operation("unwind Zig thread", scenario.handle().backtrace())
            .await;
        assert_eq!(trace.thread, thread.id);
        assert!(!trace.frames.is_empty());
        let Some(frame) = trace.frames.iter().find(|frame| {
            frame
                .function
                .as_ref()
                .is_some_and(|function| ["worker", "workerBreakpoint"].contains(&&*function.name))
        }) else {
            continue;
        };
        let variables = scenario
            .operation(
                "Zig worker variables",
                scenario
                    .handle()
                    .at(uscope::StopContext {
                        stop,
                        thread: thread.id,
                        frame: frame.id,
                    })
                    .variables(),
            )
            .await;
        let value = variables
            .variables
            .iter()
            .find(|variable| &*variable.name == "value")
            .unwrap_or_else(|| panic!("no Zig worker value in {variables:?}"));
        let uscope::VariableValue::Scalar(ScalarValue::Unsigned(value)) =
            available_value(&value.state)
        else {
            panic!("Zig worker value was not available: {value:?}");
        };
        values.insert(*value);
    }
    assert_eq!(values, BTreeSet::from([101, 202]));

    for _ in 0..3 {
        if matches!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        ) {
            assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
            return;
        }
    }
    panic!("Zig threads did not exit after repairing co-hit breakpoints");
}

#[tokio::test]
async fn parameters_use_live_values_and_participate_in_lexical_shadowing() {
    let mut changing = Scenario::new(
        "changing parameter",
        Scenario::fixture("variables-parameters-gcc-o0"),
    );
    changing
        .add_source_breakpoint("variables-parameters.c", 44)
        .await;
    assert!(matches!(
        changing.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let first = changing
        .operation(
            "first changing parameter",
            changing.handle().variable("changing"),
        )
        .await;
    assert_variable_value(&first, ScalarValue::Signed(17));
    assert_eq!(first.kind, VariableKind::Parameter);
    assert!(matches!(
        changing.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let second = changing
        .operation(
            "second changing parameter",
            changing.handle().variable("changing"),
        )
        .await;
    assert_variable_value(&second, ScalarValue::Signed(24));
    changing.shutdown().await;

    let mut shadow = Scenario::new(
        "parameter shadowing",
        Scenario::fixture("variables-parameters-gcc-o0"),
    );
    shadow
        .add_source_breakpoint("variables-parameters.c", 36)
        .await;
    assert!(matches!(
        shadow.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let named = shadow
        .operation("shadowing local", shadow.handle().variable("shadowed"))
        .await;
    assert_eq!(named.kind, VariableKind::Local);
    assert_variable_value(&named, ScalarValue::Signed(200));
    let listed = shadow
        .operation("parameter and shadow", shadow.handle().variables())
        .await;
    assert_eq!(listed.variables.len(), 2);
    assert_eq!(listed.variables[0].kind, VariableKind::Parameter);
    assert_variable_value(&listed.variables[0], ScalarValue::Signed(100));
    assert_eq!(listed.variables[1].kind, VariableKind::Local);
    assert_variable_value(&listed.variables[1], ScalarValue::Signed(200));
    shadow.shutdown().await;
}

#[tokio::test]
async fn variable_inspection_follows_the_selected_inline_frame() {
    for fixture in ["variables-inline-gcc-o0", "variables-inline-clang-o0"] {
        let mut scenario = Scenario::launch(fixture);
        scenario
            .add_source_breakpoint("variables-inline.c", 11)
            .await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        // Physical frame: the caller's parameter and locals are visible, the
        // inline instance's data objects are not.
        let listed = scenario
            .operation("caller-scope variables", scenario.handle().variables())
            .await;
        assert_eq!(listed.frame, uscope::PresentedFrame::Physical, "{fixture}");
        let names = listed
            .variables
            .iter()
            .map(|variable| variable.name.as_ref())
            .collect::<Vec<_>>();
        assert!(
            names.contains(&"value")
                && names.contains(&"caller_local")
                && !names.contains(&"inline_local"),
            "{fixture} caller scope leaked inline data objects: {names:?}"
        );
        let caller_parameter = scenario
            .operation("caller parameter", scenario.handle().variable("value"))
            .await;
        assert_eq!(caller_parameter.kind, VariableKind::Parameter);
        assert_variable_value(&caller_parameter, ScalarValue::Signed(7));

        // Step into the inline body (line 6, after inline_local is assigned).
        step_to_source_line(&mut scenario, 6).await;
        let snapshot = scenario.snapshot().await;
        assert!(
            matches!(
                snapshot
                    .presentation
                    .as_ref()
                    .expect("stopped presentation")
                    .frame,
                uscope::PresentedFrame::Inline(_)
            ),
            "{fixture} did not present the inline frame: {snapshot:?}"
        );

        // Inline frame: only the inline instance's parameter and local are in scope.
        let inline_parameter = scenario
            .operation("inline parameter", scenario.handle().variable("value"))
            .await;
        assert_eq!(inline_parameter.kind, VariableKind::Parameter);
        assert_variable_value(&inline_parameter, ScalarValue::Signed(8));
        let inline_local = scenario
            .operation("inline local", scenario.handle().variable("inline_local"))
            .await;
        assert_variable_value(&inline_local, ScalarValue::Signed(11));
        assert!(
            matches!(
                scenario.handle().variable("caller_local").await,
                Err(Error::VariableNotFound(name)) if name == "caller_local"
            ),
            "{fixture} exposed a caller local inside the inline frame"
        );
        let listed = scenario
            .operation("inline-scope variables", scenario.handle().variables())
            .await;
        assert_eq!(
            listed.frame,
            snapshot.presentation.expect("stopped presentation").frame,
            "{fixture} variable snapshot did not identify the selected inline instance"
        );
        let names = listed
            .variables
            .iter()
            .map(|variable| variable.name.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(names, ["value", "inline_local"], "{fixture}");

        // Back in the caller after the inline returns: the caller's locals
        // are visible again and the inline local is out of scope.
        step_to_source_line(&mut scenario, 13).await;
        let caller_local = scenario
            .operation("caller local", scenario.handle().variable("caller_local"))
            .await;
        assert_variable_value(&caller_local, ScalarValue::Signed(8));
        let inline_result = scenario
            .operation("inline result", scenario.handle().variable("inline_result"))
            .await;
        assert_variable_value(&inline_result, ScalarValue::Signed(22));
        assert!(matches!(
            scenario.handle().variable("inline_local").await,
            Err(Error::VariableNotFound(name)) if name == "inline_local"
        ));

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn optimized_inline_variables_preserve_scope_and_computed_values() {
    for fixture in ["variables-inline-gcc-o1", "variables-inline-clang-o1"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("inline_target").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let stopped = scenario.snapshot().await;
        assert!(matches!(
            stopped
                .presentation
                .as_ref()
                .expect("stopped presentation")
                .frame,
            uscope::PresentedFrame::Inline(_)
        ));

        let listed = scenario
            .operation("optimized inline variables", scenario.handle().variables())
            .await;
        assert_eq!(
            listed.frame,
            stopped.presentation.expect("stopped presentation").frame,
            "{fixture} variable snapshot did not identify the selected inline instance"
        );
        let names = listed
            .variables
            .iter()
            .map(|variable| variable.name.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(names, ["value", "inline_local"], "{fixture}: {listed:?}");
        assert_eq!(listed.variables[0].kind, VariableKind::Parameter);
        assert_eq!(listed.variables[1].kind, VariableKind::Local);
        assert_variable_value(&listed.variables[0], ScalarValue::Signed(8));
        assert_variable_value(&listed.variables[1], ScalarValue::Signed(11));
        assert!(
            listed.variables.iter().all(|variable| matches!(
                variable.state,
                VariableState::Available {
                    source: uscope::VariableValueSource::Computed,
                    ..
                }
            )),
            "{fixture}: {listed:?}"
        );
        let parameter = scenario
            .operation(
                "optimized inline parameter",
                scenario.handle().variable("value"),
            )
            .await;
        assert_eq!(parameter, listed.variables[0]);
        assert!(matches!(
            scenario.handle().variable("caller_local").await,
            Err(Error::VariableNotFound(name)) if name == "caller_local"
        ));

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_line_breakpoint_inside_an_inline_body_presents_the_inline_frame() {
    let mut scenario = Scenario::new(
        "inline line stop",
        Scenario::fixture("variables-inline-gcc-o0"),
    );
    // The line is the inline instance's code, as gdb presents it, not also
    // its caller's.
    scenario
        .add_source_breakpoint("variables-inline.c", 6)
        .await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let names = trace
        .frames
        .iter()
        .take(2)
        .map(|frame| {
            frame
                .function
                .as_ref()
                .map(|function| function.name.to_string())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            Some("inline_target".to_owned()),
            Some("inline_caller".to_owned())
        ]
    );
    let value = scenario
        .operation("value", scenario.handle().variable("value"))
        .await;
    assert_variable_value(&value, ScalarValue::Signed(8));
    scenario.shutdown().await;
}

#[tokio::test]
async fn variable_inspection_requires_a_stopped_inferior() {
    let mut scenario = Scenario::new("variable state errors", Scenario::fixture("spin"));
    assert!(matches!(
        scenario.handle().variables().await,
        Err(Error::NotRunning)
    ));
    let run = scenario.start_running().await;
    assert!(matches!(
        scenario.handle().variables().await,
        Err(Error::NotStopped)
    ));
    scenario.shutdown().await;
    let _ = run.await.expect("run task panicked");
}

#[tokio::test]
async fn variable_inspection_uses_the_selected_threads_stack() {
    let mut scenario = Scenario::new(
        "thread-local variable inspection",
        Scenario::fixture("variables-threads"),
    );
    scenario
        .add_source_breakpoint("variables-threads.c", 23)
        .await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let threads = scenario.snapshot().await.threads.clone();
    let mut values = BTreeSet::new();
    for thread in threads.iter() {
        scenario
            .operation(
                "select stopped thread",
                scenario.handle().select_thread(thread.id),
            )
            .await;
        match scenario.handle().variable("thread_value").await {
            Ok(variable) => {
                let uscope::VariableValue::Scalar(ScalarValue::Signed(value)) =
                    available_value(&variable.state)
                else {
                    panic!("thread_value was not a signed available scalar: {variable:?}");
                };
                values.insert(*value);
            }
            Err(Error::LocationUnavailable | Error::VariableNotFound(_)) => {}
            Err(error) => panic!("unexpected thread variable error: {error}"),
        }
    }
    assert_eq!(values, BTreeSet::from([101, 202]));
    scenario.shutdown().await;
}
