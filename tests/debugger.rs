mod support;

use std::collections::{BTreeMap, BTreeSet};

use uscope::{
    Architecture, BreakpointLocation, ByteOrder, CodeInstanceKind, Debugger, EntryProvenance,
    Error, ExitStatus, InferiorState, InlineFrameLookup, ModuleImage, PointerWidth, RegisterRole,
    ScalarValue, SourceContext, SourceFile, SourceLocation, StepKind, StopReason, ThreadState,
    UnwindTermination, VariableState, VirtualAddress,
};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use support::Scenario;
use tokio::time::{Duration, timeout};

fn single_image_breakpoint_address(breakpoint: &uscope::Breakpoint) -> uscope::ImageAddress {
    assert_eq!(breakpoint.locations.len(), 1);
    match breakpoint.locations[0].location {
        BreakpointLocation::Image(address) => address,
        BreakpointLocation::Virtual(_) => panic!("function breakpoint was not image-based"),
    }
}

#[tokio::test]
async fn stack_scalar_variables_are_read_through_the_public_scenario_path() {
    for fixture in ["variables-gcc-o0", "variables-clang-o0"] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_source_breakpoint("variables.c", 52).await;
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
                expected_size
            );
            let VariableState::Available { storage, raw, .. } = &variable.state else {
                unreachable!("value assertion checked availability")
            };
            assert!(
                matches!(storage, uscope::VariableStorage::Memory(address) if address.get() != 0)
            );
            assert_eq!(raw.len(), usize::try_from(expected_size).unwrap());
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
async fn variable_inspection_uses_live_values_and_lexical_scope() {
    let mut changing = Scenario::new(
        "changing stack variable",
        Scenario::fixture("variables-gcc-o0"),
    );
    changing.add_source_breakpoint("variables.c", 11).await;
    assert!(matches!(
        changing.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let first = changing
        .operation(
            "first changing value",
            changing.handle().variable("changing"),
        )
        .await;
    assert_variable_value(&first, ScalarValue::Signed(10));
    assert!(matches!(
        changing.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let second = changing
        .operation(
            "second changing value",
            changing.handle().variable("changing"),
        )
        .await;
    assert_variable_value(&second, ScalarValue::Signed(17));
    changing.shutdown().await;

    let mut shadow = Scenario::new(
        "shadowed stack variables",
        Scenario::fixture("variables-gcc-o0"),
    );
    shadow.add_source_breakpoint("variables.c", 20).await;
    assert!(matches!(
        shadow.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let named = shadow
        .operation("innermost shadow", shadow.handle().variable("shadowed"))
        .await;
    assert_variable_value(&named, ScalarValue::Signed(200));
    let listed = shadow
        .operation("all shadows", shadow.handle().variables())
        .await;
    assert_eq!(listed.variables.len(), 2);
    assert_variable_value(&listed.variables[0], ScalarValue::Signed(100));
    assert_variable_value(&listed.variables[1], ScalarValue::Signed(200));
    shadow.shutdown().await;
}

#[tokio::test]
async fn variable_inspection_reports_partial_support_and_parameters_honestly() {
    let mut partial = Scenario::new(
        "partial variable support",
        Scenario::fixture("variables-gcc-o0"),
    );
    partial.add_source_breakpoint("variables.c", 29).await;
    assert!(matches!(
        partial.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let variables = partial
        .operation("partial variables", partial.handle().variables())
        .await;
    assert_eq!(variables.variables.len(), 2);
    assert_variable_value(&variables.variables[0], ScalarValue::Signed(42));
    assert!(matches!(
        variables.variables[1].state,
        VariableState::Unavailable(_)
    ));
    partial.shutdown().await;

    let mut parameter = Scenario::new(
        "unsupported parameter",
        Scenario::fixture("variables-gcc-o0"),
    );
    parameter.add_source_breakpoint("variables.c", 5).await;
    assert!(matches!(
        parameter.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    assert!(matches!(
        parameter.handle().variable("parameter").await,
        Err(Error::ParameterUnsupported(name)) if name == "parameter"
    ));
    assert!(matches!(
        parameter.handle().variable("missing").await,
        Err(Error::VariableNotFound(name)) if name == "missing"
    ));
    parameter.shutdown().await;
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
                let VariableState::Available {
                    value: ScalarValue::Signed(value),
                    ..
                } = variable.state
                else {
                    panic!("thread_value was not a signed available scalar: {variable:?}");
                };
                values.insert(value);
            }
            Err(Error::LocationUnavailable | Error::VariableNotFound(_)) => {}
            Err(error) => panic!("unexpected thread variable error: {error}"),
        }
    }
    assert_eq!(values, BTreeSet::from([101, 202]));
    scenario.shutdown().await;
}

fn assert_variable_value(variable: &uscope::Variable, expected: impl Into<ScalarValue>) {
    let VariableState::Available { value, .. } = &variable.state else {
        panic!("{} was not available: {:?}", variable.name, variable.state);
    };
    assert_eq!(*value, expected.into());
}

#[tokio::test]
async fn source_line_breakpoint_stops_through_the_public_scenario_path() {
    let mut scenario = Scenario::new("source line breakpoint", Scenario::fixture("basic"));
    let breakpoint = scenario.add_source_breakpoint("basic.c", 11).await;
    assert_eq!(breakpoint.locations.len(), 1);

    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let context = scenario
        .operation("source context", scenario.handle().source_context(0))
        .await;
    assert_eq!(context.location.line.get(), 11);
    assert!(context.file.path.ends_with("tests/fixtures/basic.c"));
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(uscope::ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn file_qualified_function_breakpoint_stops_at_the_selected_function() {
    let mut scenario = Scenario::new("file function breakpoint", Scenario::fixture("basic"));
    scenario
        .add_file_function_breakpoint("tests/fixtures/basic.c", "breakpoint_target")
        .await;

    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let context = scenario
        .operation("source context", scenario.handle().source_context(0))
        .await;
    assert_eq!(context.location.line.get(), 5);
    scenario.shutdown().await;
}

#[tokio::test]
async fn breakpoint_deletion_preserves_shared_sites_and_stopped_instruction_execution() {
    let mut scenario = Scenario::new("breakpoint deletion", Scenario::fixture("basic"));
    let function = scenario.add_breakpoint("breakpoint_target").await;
    let source = scenario.add_source_breakpoint("basic.c", 5).await;
    assert_eq!(function.locations[0].location, source.locations[0].location);

    let revision = scenario.snapshot().await.revision;
    assert_eq!(scenario.remove_breakpoint(function.id).await, function);
    let snapshot = scenario.snapshot().await;
    assert_eq!(snapshot.revision, revision + 1);
    assert_eq!(snapshot.breakpoints.as_ref(), std::slice::from_ref(&source));

    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    scenario.remove_breakpoint(source.id).await;
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(uscope::ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn deleting_all_breakpoints_is_one_coherent_public_mutation() {
    let mut scenario = Scenario::new("delete all breakpoints", Scenario::fixture("basic"));
    let first = scenario.add_breakpoint("main").await;
    let second = scenario.add_breakpoint("breakpoint_target").await;
    let revision = scenario.snapshot().await.revision;

    assert_eq!(scenario.remove_all_breakpoints().await, vec![first, second]);
    let snapshot = scenario.snapshot().await;
    assert_eq!(snapshot.revision, revision + 1);
    assert!(snapshot.breakpoints.is_empty());
    assert_eq!(
        scenario.run_to_stop().await,
        StopReason::Exited(uscope::ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn deleting_an_unknown_breakpoint_does_not_mutate_public_state() {
    let mut scenario = Scenario::new("unknown breakpoint deletion", Scenario::fixture("basic"));
    scenario.add_breakpoint("main").await;
    let before = scenario.snapshot().await;

    let error = scenario
        .handle()
        .remove_breakpoint(uscope::BreakpointId::new(999))
        .await
        .expect_err("unknown breakpoint must fail");
    assert!(matches!(error, uscope::Error::BreakpointNotFound(999)));
    let after = scenario.snapshot().await;
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.breakpoints, before.breakpoints);
    scenario.shutdown().await;
}

#[tokio::test]
async fn unresolved_source_breakpoints_fail_without_mutating_public_state() {
    let mut scenario = Scenario::new("unresolved source breakpoint", Scenario::fixture("basic"));
    let before = scenario.snapshot().await;
    let missing_file = scenario
        .handle()
        .add_breakpoint(uscope::BreakpointSpec::Source {
            path: "missing.c".into(),
            line: uscope::LineNumber::new(1).unwrap(),
        })
        .await;
    assert!(matches!(missing_file, Err(Error::SourceFileNotFound(_))));
    let missing_line = scenario
        .handle()
        .add_breakpoint(uscope::BreakpointSpec::Source {
            path: "basic.c".into(),
            line: uscope::LineNumber::new(999).unwrap(),
        })
        .await;
    assert!(matches!(
        missing_line,
        Err(Error::SourceLineUnavailable { .. })
    ));
    let after = scenario.snapshot().await;
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.breakpoints, before.breakpoints);
    scenario.shutdown().await;
}

#[tokio::test]
async fn deleting_breakpoints_while_running_is_rejected_without_mutation() {
    let mut scenario = Scenario::new("delete while running", Scenario::fixture("spin"));
    let breakpoint = scenario.add_breakpoint("unreached").await;
    let run = scenario.start_running().await;
    let before = scenario.snapshot().await;
    let mut events = scenario.handle().subscribe();

    assert!(matches!(
        scenario.handle().remove_breakpoint(breakpoint.id).await,
        Err(Error::NotStopped)
    ));
    let after = scenario.snapshot().await;
    assert_eq!(after.breakpoints, before.breakpoints);
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event, uscope::DebuggerEvent::BreakpointsChanged { .. }),
            "rejected deletion published a breakpoint mutation"
        );
    }

    scenario.shutdown().await;
    let _shutdown_result = run.await.expect("run task panicked");
}

#[tokio::test]
async fn breakpoint_memory_and_event_state_follow_one_consistent_scenario() {
    let mut scenario = Scenario::new("breakpoint lifecycle", Scenario::fixture("basic"));

    assert!(matches!(
        scenario.handle().registers().await,
        Err(Error::NotRunning)
    ));

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

    let source = location
        .image
        .source
        .as_ref()
        .expect("source location")
        .clone();
    let source_file = scenario
        .handle()
        .module_image()
        .source_file(source.file)
        .expect("source file")
        .clone();

    assert!(source_file.path.ends_with("basic.c"));
    assert!(source_file.path.is_absolute());
    assert!(source.line.get() > 0);

    let context = scenario
        .operation("source context", scenario.handle().source_context(3))
        .await;

    assert_basic_source_context(&context, &source_file, &source);

    let image_address = single_image_breakpoint_address(&breakpoint);

    assert_ne!(
        first_address.get(),
        image_address.get(),
        "PIE was not relocated"
    );

    let snapshot = scenario.snapshot().await;

    assert_eq!(snapshot.revision, scenario.last_revision());
    assert!(matches!(
        &snapshot.inferior,
        InferiorState::Stopped { reason, .. } if *reason == first
    ));
    assert_eq!(
        snapshot.breakpoints.as_ref(),
        std::slice::from_ref(&breakpoint)
    );

    let registers = scenario
        .operation("read registers", scenario.handle().registers())
        .await;

    assert_register_snapshot(&registers, &snapshot, first_address);

    let duplicate = scenario.add_breakpoint("breakpoint_target").await;

    assert_eq!(duplicate, breakpoint);
    assert_eq!(
        scenario.snapshot().await.breakpoints.as_ref(),
        std::slice::from_ref(&breakpoint)
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
    assert!(matches!(
        running.handle().registers().await,
        Err(Error::NotStopped)
    ));

    let status = running.shutdown().await.expect("inferior exit event");

    assert!(matches!(
        status,
        ExitStatus::Terminated(exception) if exception.code == 9
    ));
    assert!(matches!(
        run.await.expect("run task"),
        Ok(StopReason::Exited(ExitStatus::Terminated(exception))) if exception.code == 9
    ));

    let mut stopped = Scenario::new("shutdown stopped", Scenario::fixture("basic"));

    stopped.add_breakpoint("breakpoint_target").await;

    assert!(matches!(
        stopped.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    stopped.shutdown().await;
}

fn register_u64(registers: &uscope::RegisterSnapshot, role: RegisterRole) -> u64 {
    let value = registers
        .registers
        .iter()
        .find(|value| value.register.role == Some(role))
        .unwrap_or_else(|| panic!("missing {role:?} register"));
    let bytes: [u8; 8] = value
        .bytes
        .as_ref()
        .try_into()
        .unwrap_or_else(|_| panic!("{} was not 64 bits", value.register.name));

    u64::from_le_bytes(bytes)
}

fn assert_basic_source_context(
    context: &SourceContext,
    source_file: &SourceFile,
    source: &SourceLocation,
) {
    let current = context
        .lines
        .iter()
        .find(|line| line.number == context.location.line)
        .expect("current source line");

    assert_eq!(&context.file, source_file);
    assert_eq!(&context.location, source);
    assert_eq!(context.location.line.get(), 5);
    assert_eq!(
        current.text.as_ref(),
        "__attribute__((noinline)) uint64_t breakpoint_target(void) {"
    );
    assert_eq!(
        context
            .lines
            .first()
            .expect("first source line")
            .number
            .get(),
        2
    );
    assert_eq!(
        context.lines.last().expect("last source line").number.get(),
        8
    );
}

fn assert_register_snapshot(
    registers: &uscope::RegisterSnapshot,
    state: &uscope::StateSnapshot,
    instruction: VirtualAddress,
) {
    assert_eq!(registers.revision, state.revision);
    assert_eq!(registers.target.architecture, Architecture::X86_64);
    assert_eq!(registers.target.byte_order, ByteOrder::Little);
    assert_eq!(registers.target.pointer_width, PointerWidth::Bits64);
    assert_eq!(
        registers.thread.get(),
        match &state.inferior {
            InferiorState::Stopped { process_id, .. } => process_id.get(),
            _ => panic!("inferior was not stopped"),
        }
    );
    assert_eq!(
        register_u64(registers, RegisterRole::ProgramCounter),
        instruction.get()
    );
    assert_ne!(register_u64(registers, RegisterRole::StackPointer), 0);
    assert_ne!(register_u64(registers, RegisterRole::FramePointer), 0);
    assert!(registers.registers.iter().any(|value| {
        value.register.name.as_ref() == "rax" && value.register.bits == 64 && value.bytes.len() == 8
    }));
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

        let source = scenario
            .operation("source context", scenario.handle().source_context(1))
            .await;

        assert!(source.file.path.ends_with("unwind.c"));
        assert!(source.file.path.is_absolute());
        assert!(
            source
                .lines
                .iter()
                .any(|line| line.text.contains("deepest")),
            "unexpected {fixture} source context: {source:?}"
        );

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
async fn dwarf_normalization_preserves_inline_instances_and_line_rows() {
    for fixture in [
        "inline-gcc-o1",
        "inline-gcc-o2",
        "inline-clang-o1",
        "inline-clang-o2",
    ] {
        let debugger = Debugger::new(Scenario::fixture(fixture)).expect("initialize debugger");
        let image = debugger.handle().module_image().clone();
        assert_inline_metadata(&image, fixture);

        debugger.shutdown().await.expect("shutdown debugger");
    }
}

fn assert_inline_metadata(image: &ModuleImage, fixture: &str) {
    let leaf = image.function_named("leaf").expect("leaf definition");
    let leaf_instances: Vec<_> = image
        .instances_for_function(leaf.id)
        .filter(|instance| matches!(instance.kind, CodeInstanceKind::Inline { .. }))
        .collect();

    assert_eq!(
        leaf_instances.len(),
        6,
        "unexpected {fixture} leaf instances"
    );
    assert_eq!(
        image
            .code_instances()
            .iter()
            .filter(|instance| matches!(instance.kind, CodeInstanceKind::Inline { .. }))
            .count(),
        9,
        "unexpected {fixture} inline instance count"
    );
    assert!(
        image.code_instances().iter().any(|instance| {
            matches!(instance.kind, CodeInstanceKind::Inline { .. }) && instance.ranges.len() > 1
        }),
        "{fixture} lost discontiguous ranges"
    );

    let mut columns_by_line = BTreeMap::<u64, BTreeSet<u64>>::new();
    for instance in &leaf_instances {
        let CodeInstanceKind::Inline {
            call_site: Some(call_site),
        } = &instance.kind
        else {
            panic!("{fixture} leaf instance has no call site")
        };

        if let Some(column) = call_site.column {
            columns_by_line
                .entry(call_site.line.get())
                .or_default()
                .insert(column.get());
        }
    }
    assert!(
        columns_by_line.values().any(|columns| columns.len() >= 2),
        "{fixture} did not preserve same-line call columns"
    );

    let nested_leaf = leaf_instances
        .iter()
        .find(|instance| {
            instance
                .parent
                .and_then(|parent| image.code_instance(parent))
                .and_then(|parent| image.function(parent.function))
                .is_some_and(|function| function.name.as_ref() == "middle")
        })
        .expect("nested leaf instance");
    let location = image.locate(nested_leaf.ranges[0].start);
    let InlineFrameLookup::Unique(chain) = location.inline_frames else {
        panic!("{fixture} did not resolve one inline chain: {location:?}")
    };
    let chain_names: Vec<_> = chain
        .instances
        .iter()
        .map(|instance| {
            let instance = image.code_instance(*instance).expect("known instance");
            image
                .function(instance.function)
                .expect("known function")
                .name
                .as_ref()
        })
        .collect();
    assert_eq!(
        chain_names,
        ["middle", "leaf"],
        "unexpected {fixture} chain"
    );

    assert!(!image.statement_rows().is_empty());
    if fixture.starts_with("inline-gcc") {
        assert!(
            image.statement_rows().windows(2).any(|rows| {
                rows[0].sequence == rows[1].sequence && rows[0].address == rows[1].address
            }),
            "{fixture} lost equal-address line rows"
        );
    }
    let expected_provenance = if fixture.starts_with("inline-gcc") {
        EntryProvenance::Explicit
    } else {
        EntryProvenance::RangeStart
    };
    assert!(leaf_instances.iter().all(|instance| {
        instance
            .breakpoint_entry
            .is_some_and(|entry| entry.provenance == expected_provenance)
    }));
}

#[tokio::test]
async fn inline_function_breakpoints_resolve_every_concrete_instance() {
    for fixture in [
        "inline-gcc-o1",
        "inline-gcc-o2",
        "inline-clang-o1",
        "inline-clang-o2",
    ] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        let expected_instances = {
            let image = scenario.handle().module_image();
            let function = image.function_named("leaf").expect("leaf function");

            image
                .instances_for_function(function.id)
                .map(|instance| instance.id)
                .collect::<BTreeSet<_>>()
        };
        let breakpoint = scenario.add_breakpoint("leaf").await;
        let resolved_instances = breakpoint
            .locations
            .iter()
            .flat_map(|location| location.code_instances.iter().copied())
            .collect::<BTreeSet<_>>();
        let resolved_addresses = breakpoint
            .locations
            .iter()
            .map(|location| location.location)
            .collect::<BTreeSet<_>>();

        assert_eq!(resolved_instances, expected_instances, "{fixture}");
        assert_eq!(
            resolved_addresses.len(),
            breakpoint.locations.len(),
            "{fixture} retained duplicate physical sites"
        );
        assert!(
            breakpoint
                .locations
                .iter()
                .all(|location| matches!(location.location, BreakpointLocation::Image(_))),
            "{fixture} function breakpoint was not image-relative"
        );

        let duplicate = scenario.add_breakpoint("leaf").await;
        assert_eq!(duplicate, breakpoint, "{fixture}");
        assert_eq!(
            scenario.snapshot().await.breakpoints.as_ref(),
            &[breakpoint],
            "{fixture} duplicated one logical breakpoint"
        );

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn inline_breakpoint_hits_select_the_matching_concrete_instance() {
    for fixture in ["inline-gcc-o2", "inline-clang-o2"] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        let breakpoint = scenario.add_breakpoint("leaf").await;
        let mut reason = scenario.run_to_stop().await;
        let mut hits = 0;
        let mut hit_instances = BTreeSet::new();

        while let StopReason::Breakpoint { .. } = reason {
            hits += 1;
            let location = scenario
                .operation(
                    "inline breakpoint location",
                    scenario.handle().current_location(),
                )
                .await;
            let snapshot = scenario.snapshot().await;
            let uscope::PresentedFrame::Inline(selected) = snapshot
                .presentation
                .as_ref()
                .expect("stopped presentation")
                .frame
            else {
                panic!("{fixture} did not select an inline frame: {snapshot:?}")
            };
            let resolved = breakpoint
                .locations
                .iter()
                .find(|resolved| {
                    resolved.location == BreakpointLocation::Image(location.image.address)
                })
                .expect("hit one resolved breakpoint location");

            assert!(resolved.code_instances.contains(&selected), "{fixture}");
            hit_instances.insert(selected);
            assert_eq!(
                location
                    .image
                    .function
                    .as_ref()
                    .map(|function| function.name.as_ref()),
                Some("leaf"),
                "{fixture}"
            );

            reason = scenario.resume_to_stop().await;
        }

        assert_eq!(reason, StopReason::Exited(ExitStatus::Code(0)), "{fixture}");
        assert_eq!(
            hits, 5,
            "{fixture} executed an unexpected set of leaf calls"
        );
        let same_line_instances = {
            let image = scenario.handle().module_image();
            hit_instances
                .iter()
                .filter(|instance| {
                    image.code_instance(**instance).is_some_and(|instance| {
                        matches!(
                            &instance.kind,
                            CodeInstanceKind::Inline {
                                call_site: Some(call_site)
                            } if call_site.line.get() == 30
                        )
                    })
                })
                .count()
        };
        assert_eq!(same_line_instances, 2, "{fixture}");
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn virtual_steps_reveal_inline_frames_without_running_the_inferior() {
    for fixture in ["inline-gcc-o2", "inline-clang-o2"] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_breakpoint("caller").await;

        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let initial = scenario.snapshot().await;
        let instruction = scenario
            .operation("caller location", scenario.handle().current_location())
            .await;
        assert_inline_location(fixture, &instruction, "caller", 28, instruction.address);

        let mut events = scenario.handle().subscribe();
        assert_eq!(
            scenario.step_to_stop(StepKind::IntoSource).await,
            StopReason::Step {
                kind: StepKind::IntoSource
            }
        );
        let middle = scenario
            .operation("middle location", scenario.handle().current_location())
            .await;
        assert_inline_location(fixture, &middle, "middle", 14, instruction.address);
        assert_no_continued_event(fixture, &mut events);

        let mut events = scenario.handle().subscribe();
        scenario.step_to_stop(StepKind::IntoSource).await;
        let leaf = scenario
            .operation("leaf location", scenario.handle().current_location())
            .await;
        assert_inline_location(fixture, &leaf, "leaf", 7, instruction.address);
        assert_no_continued_event(fixture, &mut events);
        assert_ne!(
            initial.stop_id,
            scenario.snapshot().await.stop_id,
            "{fixture}"
        );

        let trace = scenario
            .operation("inline backtrace", scenario.handle().backtrace())
            .await;
        assert_inline_backtrace(fixture, &trace);

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn next_skips_inline_descendants_of_the_selected_caller() {
    for fixture in ["inline-gcc-o2", "inline-clang-o2"] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_breakpoint("caller").await;
        scenario.run_to_stop().await;

        assert_eq!(
            scenario.step_to_stop(StepKind::OverSource).await,
            StopReason::Step {
                kind: StepKind::OverSource
            },
            "{fixture}"
        );
        let location = scenario
            .operation(
                "location after inline next",
                scenario.handle().current_location(),
            )
            .await;
        assert_eq!(
            location
                .image
                .function
                .as_ref()
                .map(|function| function.name.as_ref()),
            Some("caller"),
            "{fixture}"
        );
        assert_eq!(
            location
                .image
                .source
                .as_ref()
                .map(|source| source.line.get()),
            Some(29),
            "{fixture}"
        );

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn inline_next_is_owned_by_the_selected_thread() {
    for fixture in ["inline-threads-gcc-o2", "inline-threads-clang-o2"] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_breakpoint("thread_caller").await;
        scenario.run_to_stop().await;
        let before = scenario.snapshot().await;
        let selected = before.selected_thread.expect("selected worker thread");

        assert_eq!(
            scenario.step_to_stop(StepKind::OverSource).await,
            StopReason::Step {
                kind: StepKind::OverSource
            },
            "{fixture}"
        );
        let after = scenario.snapshot().await;
        let location = scenario
            .operation(
                "thread inline next location",
                scenario.handle().current_location(),
            )
            .await;

        assert_eq!(after.selected_thread, Some(selected), "{fixture}");
        assert_eq!(
            location
                .image
                .function
                .as_ref()
                .map(|function| function.name.as_ref()),
            Some("thread_caller"),
            "{fixture}"
        );
        assert_eq!(
            location
                .image
                .source
                .as_ref()
                .map(|source| source.line.get()),
            Some(19),
            "{fixture}"
        );
        assert!(
            after
                .threads
                .iter()
                .filter(|thread| thread.id != selected)
                .all(|thread| matches!(thread.state, ThreadState::Stopped { .. })),
            "{fixture} resumed a non-selected thread"
        );

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn finish_exits_inline_instances_without_unwinding_the_physical_frame() {
    for fixture in ["inline-gcc-o2", "inline-clang-o2"] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_breakpoint("caller").await;
        scenario.run_to_stop().await;
        scenario.step_to_stop(StepKind::IntoSource).await;
        scenario.step_to_stop(StepKind::IntoSource).await;

        assert_eq!(
            scenario.step_to_stop(StepKind::Out).await,
            StopReason::Step {
                kind: StepKind::Out
            },
            "{fixture}"
        );
        let location = scenario
            .operation(
                "location after inline finish",
                scenario.handle().current_location(),
            )
            .await;
        assert_eq!(
            location
                .image
                .function
                .as_ref()
                .map(|function| function.name.as_ref()),
            Some("caller"),
            "{fixture}"
        );
        assert_eq!(
            location
                .image
                .source
                .as_ref()
                .map(|source| source.line.get()),
            Some(29),
            "{fixture}"
        );

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn instruction_step_moves_the_pc_before_rebuilding_inline_presentation() {
    for fixture in ["inline-gcc-o2", "inline-clang-o2"] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_breakpoint("caller").await;
        scenario.run_to_stop().await;
        let before = scenario
            .operation(
                "location before stepi",
                scenario.handle().current_location(),
            )
            .await;

        assert_eq!(
            scenario.step_to_stop(StepKind::Instruction).await,
            StopReason::Step {
                kind: StepKind::Instruction
            },
            "{fixture}"
        );
        let after = scenario
            .operation("location after stepi", scenario.handle().current_location())
            .await;
        let snapshot = scenario.snapshot().await;

        assert_ne!(after.address, before.address, "{fixture}");
        assert_eq!(
            snapshot
                .presentation
                .as_ref()
                .map(|presentation| presentation.instruction),
            Some(after.address),
            "{fixture}"
        );

        scenario.shutdown().await;
    }
}

fn assert_inline_location(
    fixture: &str,
    location: &uscope::ExecutionLocation,
    function: &str,
    line: u64,
    address: VirtualAddress,
) {
    assert_eq!(location.address, address, "{fixture}");
    assert_eq!(
        location
            .image
            .function
            .as_ref()
            .map(|function| function.name.as_ref()),
        Some(function),
        "{fixture}"
    );
    assert_eq!(
        location
            .image
            .source
            .as_ref()
            .map(|source| source.line.get()),
        Some(line),
        "{fixture}"
    );
}

fn assert_no_continued_event(
    fixture: &str,
    events: &mut tokio::sync::broadcast::Receiver<uscope::DebuggerEvent>,
) {
    assert!(
        std::iter::from_fn(|| events.try_recv().ok())
            .all(|event| !matches!(event, uscope::DebuggerEvent::InferiorContinued { .. })),
        "{fixture} virtual step emitted InferiorContinued"
    );
}

fn assert_inline_backtrace(fixture: &str, trace: &uscope::Backtrace) {
    let frames: Vec<_> = trace
        .frames
        .iter()
        .filter_map(|frame| {
            frame
                .function
                .as_ref()
                .map(|function| (frame, function.name.as_ref()))
        })
        .collect();

    assert_eq!(
        frames
            .iter()
            .take(4)
            .map(|(_, name)| *name)
            .collect::<Vec<_>>(),
        ["leaf", "middle", "caller", "main"],
        "unexpected {fixture} frames: {trace:?}"
    );
    assert!(frames[..2].iter().all(|(frame, _)| {
        frame.kind == uscope::FrameKind::Inline && frame.code_instance.is_some()
    }));
    assert_eq!(frames[2].0.kind, uscope::FrameKind::Physical);
    assert_eq!(
        frames[..3]
            .iter()
            .map(|(frame, _)| frame.instruction)
            .collect::<Vec<_>>(),
        vec![frames[0].0.instruction; 3]
    );
    assert_eq!(
        frames[..3]
            .iter()
            .map(|(frame, _)| frame.source.as_ref().map(|source| source.line.get()))
            .collect::<Vec<_>>(),
        [Some(7), Some(14), Some(28)]
    );
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

#[tokio::test]
async fn linux_wait_ownership_allows_only_one_session_per_host_process() {
    let fixture = Scenario::fixture("basic");
    let first = Debugger::new(&fixture).expect("initialize first debugger");
    assert!(matches!(Debugger::new(&fixture), Err(Error::Backend(_))));

    first.shutdown().await.expect("shut down first debugger");

    let replacement = Debugger::new(&fixture).expect("initialize replacement debugger");
    replacement
        .shutdown()
        .await
        .expect("shut down replacement debugger");
}

#[tokio::test]
async fn pthread_breakpoint_establishes_a_coherent_all_stop_snapshot() {
    let mut scenario = Scenario::new("pthread all-stop", Scenario::fixture("threads"));
    scenario.add_breakpoint("worker_breakpoint").await;

    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    let snapshot = scenario.snapshot().await;
    let (process_id, selected) = match snapshot.inferior {
        InferiorState::Stopped {
            process_id,
            thread_id,
            all_threads_stopped: true,
            ..
        } => (process_id, thread_id),
        other => panic!("expected all-stop snapshot, got {other:?}"),
    };

    assert_ne!(
        selected.get(),
        process_id.get(),
        "worker thread was selected"
    );
    assert_eq!(snapshot.threads.len(), 3);
    assert!(
        snapshot
            .threads
            .iter()
            .all(|thread| { matches!(thread.state, ThreadState::Stopped { .. }) })
    );

    for thread in snapshot.threads.iter() {
        scenario
            .operation(
                "select stopped thread",
                scenario.handle().select_thread(thread.id),
            )
            .await;
        let registers = scenario
            .operation("inspect stopped thread", scenario.handle().registers())
            .await;
        let backtrace = scenario
            .operation("unwind stopped thread", scenario.handle().backtrace())
            .await;
        assert_eq!(registers.thread, thread.id);
        assert_eq!(backtrace.thread, thread.id);
        assert!(!backtrace.frames.is_empty());
    }
    scenario
        .operation(
            "restore selected worker",
            scenario.handle().select_thread(selected),
        )
        .await;

    let counter = scenario
        .operation(
            "resolve thread_counter",
            scenario.handle().runtime_address("thread_counter"),
        )
        .await;
    let first = scenario
        .operation("read stopped counter", scenario.handle().read_word(counter))
        .await;
    tokio::task::yield_now().await;
    let second = scenario
        .operation(
            "reread stopped counter",
            scenario.handle().read_word(counter),
        )
        .await;
    assert_eq!(first, second, "shared memory changed during all-stop");

    for _ in 0..3 {
        if matches!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        ) {
            scenario.shutdown().await;
            return;
        }
    }
    panic!("pthread fixture did not exit after repairing worker breakpoints");
}

#[tokio::test]
async fn repeated_thread_creation_and_exit_loses_no_breakpoint_events() {
    let mut scenario = Scenario::new("thread registry churn", Scenario::fixture("thread-stress"));
    scenario.add_breakpoint("churn_breakpoint").await;
    let mut lagging = scenario.handle().subscribe();

    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    for iteration in 1..64 {
        assert!(
            matches!(
                scenario.resume_to_stop().await,
                StopReason::Breakpoint { .. }
            ),
            "missing breakpoint for iteration {iteration}"
        );
    }
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    assert!(matches!(
        lagging.recv().await,
        Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
    ));
    assert!(matches!(
        scenario.snapshot().await.inferior,
        InferiorState::NotRunning
    ));

    scenario.shutdown().await;
}

#[tokio::test]
async fn a_thread_scoped_continue_stops_cleanly_when_its_thread_exits() {
    let mut scenario = Scenario::new("thread scoped exit", Scenario::fixture("threads"));
    scenario.add_breakpoint("worker_breakpoint").await;
    scenario.run_to_stop().await;

    let snapshot = scenario.snapshot().await;
    let (process, stop, thread) = match snapshot.inferior {
        InferiorState::Stopped {
            process_id,
            stop_id,
            thread_id,
            ..
        } => (process_id, stop_id, thread_id),
        other => panic!("expected stopped inferior, got {other:?}"),
    };
    let mut events = scenario.handle().subscribe();
    let execution = scenario
        .operation(
            "continue one worker",
            scenario.handle().continue_execution(
                stop,
                uscope::ResumeScope::Thread(thread),
                uscope::ExceptionDisposition::Pass,
            ),
        )
        .await;
    let reason = timeout(Duration::from_secs(2), async {
        loop {
            if let uscope::DebuggerEvent::InferiorStopped {
                execution_id: Some(event_execution),
                reason,
                ..
            } = events.recv().await.expect("event stream closed")
                && event_execution == execution
            {
                break reason;
            }
        }
    })
    .await
    .expect("thread exit stop timed out");
    assert!(matches!(
        reason,
        StopReason::ThreadExited {
            thread_id,
            status: ExitStatus::Code(0),
        } if thread_id == thread
    ));
    let stopped = scenario.snapshot().await;
    assert!(matches!(
        stopped.inferior,
        InferiorState::Stopped {
            process_id,
            all_threads_stopped: true,
            ..
        } if process_id == process
    ));
    scenario.drain_pending_events();

    for _ in 0..2 {
        if matches!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        ) {
            scenario.shutdown().await;
            return;
        }
    }
    panic!("remaining threads did not exit");
}

#[tokio::test]
async fn signal_delivery_is_preserved_and_user_sigtrap_is_not_a_breakpoint() {
    let mut scenario = Scenario::new("signal pass", Scenario::fixture("signals"));
    scenario.add_breakpoint("signal_point").await;
    scenario.run_to_stop().await;

    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exception(exception) if exception.code == 10
    ));
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exception(exception) if exception.code == 5
    ));
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;

    let mut suppressed = Scenario::new("signal suppress", Scenario::fixture("signals"));
    suppressed.add_breakpoint("signal_point").await;
    suppressed.run_to_stop().await;
    assert!(matches!(
        suppressed.resume_to_stop().await,
        StopReason::Exception(exception) if exception.code == 10
    ));
    assert!(matches!(
        suppressed
            .resume_with_exception(uscope::ExceptionDisposition::Suppress)
            .await,
        StopReason::Exception(exception) if exception.code == 5
    ));
    assert_eq!(
        suppressed.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(42))
    );
    suppressed.shutdown().await;
}

#[tokio::test]
async fn instruction_step_explicitly_delivers_a_pending_signal() {
    let mut scenario = Scenario::new("step pending signal", Scenario::fixture("signals"));
    scenario.add_breakpoint("signal_point").await;
    scenario.run_to_stop().await;
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exception(exception) if exception.code == 10
    ));

    assert_eq!(
        scenario.step_to_stop(StepKind::Instruction).await,
        StopReason::Step {
            kind: StepKind::Instruction
        }
    );
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exception(exception) if exception.code == 5
    ));
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );

    scenario.shutdown().await;
}

#[tokio::test]
async fn synchronous_faults_are_retained_and_delivered() {
    let mut scenario = Scenario::new("fatal signal", Scenario::fixture("fatal-signal"));
    scenario.add_breakpoint("fault").await;
    scenario.run_to_stop().await;

    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exception(exception) if exception.code == 11
    ));
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Terminated(exception)) if exception.code == 11
    ));

    scenario.shutdown().await;
}

#[tokio::test]
async fn job_control_stops_are_classified_without_inventing_a_pending_signal() {
    let mut scenario = Scenario::new("job control", Scenario::fixture("job-control"));

    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Exception(exception) if exception.code == 19
    ));
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exception(exception) if exception.code == 19
    ));

    let process = match scenario.snapshot().await.inferior {
        InferiorState::Stopped { process_id, .. } => process_id,
        other => panic!("expected stopped inferior, got {other:?}"),
    };
    kill(
        Pid::from_raw(i32::try_from(process.get()).expect("process ID fits i32")),
        Signal::SIGCONT,
    )
    .expect("continue stopped process group");

    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exception(exception) if exception.code == 18
    ));
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );

    scenario.shutdown().await;
}

#[tokio::test]
async fn instruction_step_executes_the_instruction_hidden_by_a_breakpoint() {
    let mut scenario = Scenario::new("instruction step", Scenario::fixture("basic"));
    scenario.add_breakpoint("breakpoint_target").await;
    let StopReason::Breakpoint { address } = scenario.run_to_stop().await else {
        panic!("expected breakpoint")
    };
    let instruction_word = scenario
        .operation(
            "read breakpoint instruction",
            scenario.handle().read_word(address),
        )
        .await;
    assert_ne!(instruction_word.to_ne_bytes()[0], 0xcc);
    scenario
        .operation(
            "rewrite breakpoint instruction",
            scenario.handle().write_word(address, instruction_word),
        )
        .await;

    assert_eq!(
        scenario.step_to_stop(StepKind::Instruction).await,
        StopReason::Step {
            kind: StepKind::Instruction
        }
    );
    let registers = scenario
        .operation("registers after step", scenario.handle().registers())
        .await;
    assert_ne!(
        register_u64(&registers, RegisterRole::ProgramCounter),
        address.get()
    );

    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    scenario.shutdown().await;
}

#[tokio::test]
async fn nonleader_exec_rewrites_the_thread_registry_and_invalidates_the_image() {
    let mut scenario = Scenario::new("nonleader exec", Scenario::fixture("thread-exec"));

    assert_eq!(scenario.run_to_stop().await, StopReason::Exec);
    let snapshot = scenario.snapshot().await;
    assert_eq!(snapshot.threads.len(), 1);
    assert!(matches!(
        scenario.handle().resume().await,
        Err(Error::Backend(_))
    ));

    scenario.shutdown().await;
}

#[tokio::test]
async fn source_step_next_and_finish_compose_over_instruction_steps() {
    let mut scenario = Scenario::new("source control", Scenario::fixture("unwind-o0"));
    scenario.add_breakpoint("deepest").await;
    scenario.run_to_stop().await;

    assert_eq!(
        scenario.step_to_stop(StepKind::IntoSource).await,
        StopReason::Step {
            kind: StepKind::IntoSource
        }
    );
    let after_step = scenario
        .operation("source after step", scenario.handle().source_context(1))
        .await;
    assert!(after_step.location.line.get() >= 6);

    assert_eq!(
        scenario.step_to_stop(StepKind::OverSource).await,
        StopReason::Step {
            kind: StepKind::OverSource
        }
    );
    assert_eq!(
        scenario.step_to_stop(StepKind::Out).await,
        StopReason::Step {
            kind: StepKind::Out
        }
    );
    let location = scenario
        .operation(
            "location after finish",
            scenario.handle().current_location(),
        )
        .await;
    assert_eq!(
        location
            .image
            .function
            .as_ref()
            .map(|function| function.name.as_ref()),
        Some("middle")
    );

    scenario.shutdown().await;
}

#[tokio::test]
async fn finish_uses_unwind_information_across_the_compiler_matrix() {
    for fixture in ["unwind-o0", "unwind-o2", "unwind-nopie", "unwind-clang-o2"] {
        let mut scenario = Scenario::new(format!("finish {fixture}"), Scenario::fixture(fixture));
        scenario.add_breakpoint("deepest").await;
        scenario.run_to_stop().await;

        assert_eq!(
            scenario.step_to_stop(StepKind::Out).await,
            StopReason::Step {
                kind: StepKind::Out
            },
            "finish failed for {fixture}"
        );
        let location = scenario
            .operation(
                "location after finish",
                scenario.handle().current_location(),
            )
            .await;
        assert_eq!(
            location
                .image
                .function
                .as_ref()
                .map(|function| function.name.as_ref()),
            Some("middle"),
            "unexpected caller for {fixture}: {location:?}"
        );

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_user_breakpoint_interrupts_finish_at_a_shared_site() {
    let mut scenario = Scenario::new("shared plan breakpoint", Scenario::fixture("unwind-o0"));
    scenario.add_breakpoint("deepest").await;
    scenario.run_to_stop().await;

    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let return_address = trace.frames[1].instruction;
    let breakpoint = scenario
        .operation(
            "add breakpoint at return address",
            scenario
                .handle()
                .add_breakpoint(uscope::BreakpointSpec::Address(return_address)),
        )
        .await;
    assert_eq!(breakpoint.locations.len(), 1);
    assert_eq!(
        breakpoint.locations[0].location,
        BreakpointLocation::Virtual(return_address)
    );

    assert_eq!(
        scenario.step_to_stop(StepKind::Out).await,
        StopReason::Breakpoint {
            address: return_address
        }
    );
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(1))
    );

    scenario.shutdown().await;
}

#[tokio::test]
async fn source_next_steps_over_calls_but_preserves_user_breakpoints() {
    let mut step_over = Scenario::new("next over call", Scenario::fixture("unwind-o0"));
    step_over.add_breakpoint("middle").await;
    step_over.run_to_stop().await;

    assert_eq!(
        step_over.step_to_stop(StepKind::OverSource).await,
        StopReason::Step {
            kind: StepKind::OverSource
        }
    );
    let location = step_over
        .operation("location after next", step_over.handle().current_location())
        .await;
    assert_eq!(
        location
            .image
            .function
            .as_ref()
            .map(|function| function.name.as_ref()),
        Some("middle")
    );
    step_over.shutdown().await;

    let mut interrupted = Scenario::new("next interruption", Scenario::fixture("unwind-o0"));
    interrupted.add_breakpoint("middle").await;
    interrupted.add_breakpoint("deepest").await;
    interrupted.run_to_stop().await;

    assert_eq!(
        interrupted.step_to_stop(StepKind::OverSource).await,
        StopReason::Step {
            kind: StepKind::OverSource
        }
    );

    assert!(matches!(
        interrupted.step_to_stop(StepKind::OverSource).await,
        StopReason::Breakpoint { .. }
    ));
    let location = interrupted
        .operation(
            "location after interrupted next",
            interrupted.handle().current_location(),
        )
        .await;
    assert_eq!(
        location
            .image
            .function
            .as_ref()
            .map(|function| function.name.as_ref()),
        Some("deepest")
    );

    interrupted.shutdown().await;
}

#[tokio::test]
async fn stale_stop_tokens_allow_exactly_one_client_to_resume() {
    let mut scenario = Scenario::new("competing clients", Scenario::fixture("basic"));
    scenario.add_breakpoint("breakpoint_target").await;
    scenario.run_to_stop().await;

    let snapshot = scenario.snapshot().await;
    let (process, stop) = match snapshot.inferior {
        InferiorState::Stopped {
            process_id,
            stop_id,
            ..
        } => (process_id, stop_id),
        other => panic!("expected stopped inferior, got {other:?}"),
    };
    let first = scenario.handle().clone();
    let second = scenario.handle().clone();
    let (first, second) = tokio::join!(
        first.continue_execution(
            stop,
            uscope::ResumeScope::Process(process),
            uscope::ExceptionDisposition::Pass,
        ),
        second.continue_execution(
            stop,
            uscope::ResumeScope::Process(process),
            uscope::ExceptionDisposition::Pass,
        ),
    );

    let accepted = usize::from(first.is_ok()) + usize::from(second.is_ok());
    assert_eq!(accepted, 1, "exactly one client must control a stop");
    let rejected = if first.is_err() { first } else { second };
    assert!(
        matches!(rejected, Err(Error::NotStopped | Error::StaleStop)),
        "unexpected competing resume result: {rejected:?}"
    );

    scenario.shutdown().await;
}

#[tokio::test]
async fn pause_cancels_an_active_source_execution_plan() {
    let mut scenario = Scenario::new("pause source plan", Scenario::fixture("step"));
    scenario.add_breakpoint("step_forever").await;
    scenario.run_to_stop().await;

    let snapshot = scenario.snapshot().await;
    let (stop, thread) = match snapshot.inferior {
        InferiorState::Stopped {
            stop_id, thread_id, ..
        } => (stop_id, thread_id),
        other => panic!("expected stopped inferior, got {other:?}"),
    };
    scenario
        .operation(
            "start nonterminating finish",
            scenario.handle().start_step(
                stop,
                thread,
                StepKind::Out,
                uscope::ExceptionDisposition::Pass,
            ),
        )
        .await;

    let reason = timeout(Duration::from_secs(2), scenario.handle().pause())
        .await
        .expect("pause timed out")
        .expect("pause failed");
    assert_eq!(reason, StopReason::Pause);
    assert!(
        scenario
            .snapshot()
            .await
            .threads
            .iter()
            .all(|thread| matches!(thread.state, ThreadState::Stopped { .. }))
    );

    scenario.shutdown().await;
}
