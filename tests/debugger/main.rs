#[path = "../support/mod.rs"]
mod support;

mod attach;
mod concurrency;
mod exec;
mod execution;
mod expressions;
#[cfg(debug_assertions)]
mod flight_recorder;
mod globals;
mod identities;
mod libraries;
mod metadata;
mod signals;
mod stepping;
mod unwind;
mod values;
mod variables;
mod views;
mod writes;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::Arc;

use uscope::{
    Architecture, BreakpointLocation, BreakpointSpec, ByteOrder, CodeInstanceKind, Debugger,
    EntryProvenance, Error, ExitStatus, InferiorState, InlineFrameLookup, LaunchOptions,
    ModuleImage, PointerWidth, ProcessId, RegisterRole, ScalarValue, SourceContext, SourceFile,
    SourceLocation, StepKind, StopReason, ThreadState, UnwindTermination, VariableKind,
    VariableState, VariableUnavailableReason, VirtualAddress,
};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use support::Scenario;
use tokio::time::{Duration, timeout};

fn available_value(state: &VariableState) -> &uscope::VariableValue {
    match state {
        VariableState::Available { value, .. } => value,
        state => panic!("value was not available: {state:?}"),
    }
}

fn available_children(state: &VariableState) -> &Arc<uscope::ValueChildrenReference> {
    match state {
        VariableState::Available {
            children: uscope::ValueChildren::Available(reference),
            ..
        } => reference,
        state => panic!("value had no available children: {state:?}"),
    }
}

async fn child_page(
    scenario: &Scenario,
    state: &VariableState,
    offset: u64,
    limit: u32,
) -> uscope::ValueChildPage {
    scenario
        .operation(
            &format!(
                "value children {offset}..{}",
                offset.saturating_add(u64::from(limit))
            ),
            scenario.handle().value_children(
                available_children(state).clone(),
                uscope::ValueChildQuery { offset, limit },
            ),
        )
        .await
}

fn named_child<'a>(page: &'a uscope::ValueChildPage, name: &str) -> &'a uscope::ValueChild {
    page.children
        .iter()
        .find(|child| {
            matches!(
                &child.relationship,
                uscope::ValueChildRelationship::Member(member)
                    if member.name.as_deref() == Some(name)
            )
        })
        .unwrap_or_else(|| panic!("value page has no member named {name}: {page:?}"))
}

fn assert_signed_state(state: &VariableState, expected: i128) {
    assert!(
        matches!(
            available_value(state),
            uscope::VariableValue::Scalar(ScalarValue::Signed(value)) if *value == expected
        ),
        "{state:?}"
    );
}

async fn record_page(
    scenario: &Scenario,
    state: &VariableState,
    minimum_children: usize,
    context: &str,
) -> uscope::ValueChildPage {
    assert!(
        matches!(
            available_value(state),
            uscope::VariableValue::Record
                | uscope::VariableValue::Union
                | uscope::VariableValue::Variant { .. }
        ),
        "{context}: value was not an aggregate: {state:?}"
    );
    let reference = available_children(state);
    assert!(
        reference.total() >= u64::try_from(minimum_children).expect("child count fits u64"),
        "{context}: aggregate had too few children: {state:?}"
    );
    let limit = u32::try_from(reference.total().min(256)).expect("bounded page fits u32");
    if limit == 0 {
        return uscope::ValueChildPage {
            stop_id: reference.stop_id(),
            offset: 0,
            total: 0,
            children: Arc::from([]),
            completion: uscope::InspectionCompletion::Complete,
            usage: uscope::InspectionUsage::default(),
        };
    }
    child_page(scenario, state, 0, limit).await
}

async fn assert_dereferenced_record(
    scenario: &Scenario,
    value: &uscope::DereferencedValue,
    minimum_children: usize,
    context: &str,
) {
    record_page(scenario, &value.state, minimum_children, context).await;
}

fn value_expression(components: &[&str]) -> uscope::Expression {
    let (first, rest) = components.split_first().expect("a name");
    rest.iter().fold(
        uscope::Expression::name(first).expect("a name"),
        |expression, member| expression.member(member).expect("a member"),
    )
}

/// The page of elements a range expression selects.
async fn evaluate_range(
    handle: &uscope::DebuggerHandle,
    text: &str,
    limits: uscope::InspectionLimits,
) -> uscope::Result<uscope::ValueChildPage> {
    match handle
        .evaluate_with(
            &parsed_value_expression(text),
            uscope::EvaluationMode::Read,
            limits,
        )
        .await?
    {
        uscope::Evaluation::Range(page) => Ok(page),
        other => panic!("`{text}` is not a range: {other:?}"),
    }
}

fn parsed_value_expression(expression: &str) -> uscope::Expression {
    uscope::Expression::parse(expression)
        .unwrap_or_else(|error| panic!("parse test expression {expression:?}: {error}"))
}

fn type_edges(kind: &uscope::TypeKind) -> Vec<uscope::TypeReference> {
    let mut edges = Vec::new();
    match kind {
        uscope::TypeKind::Enumeration { underlying, .. } => {
            edges.extend(underlying.iter().copied());
        }
        uscope::TypeKind::Pointer { target, .. } | uscope::TypeKind::Named { target, .. } => {
            edges.extend(target.iter().copied());
        }
        uscope::TypeKind::Reference { target, .. } | uscope::TypeKind::Modified { target, .. } => {
            edges.push(*target);
        }
        uscope::TypeKind::Array { element, .. } | uscope::TypeKind::Slice { element, .. } => {
            edges.push(*element);
        }
        uscope::TypeKind::Record { members, bases, .. } => {
            edges.extend(members.iter().map(|member| member.type_ref));
            edges.extend(bases.iter().map(|base| base.type_ref));
        }
        uscope::TypeKind::Union { members, .. } => {
            edges.extend(members.iter().map(|member| member.type_ref));
        }
        uscope::TypeKind::Variant {
            common_members,
            bases,
            discriminant,
            variants,
            ..
        } => {
            edges.extend(common_members.iter().map(|member| member.type_ref));
            edges.extend(bases.iter().map(|base| base.type_ref));
            match discriminant.as_ref() {
                uscope::VariantDiscriminant::Stored(member) => edges.push(member.type_ref),
                uscope::VariantDiscriminant::TagType(reference) => edges.push(*reference),
                _ => {}
            }
            edges.extend(
                variants
                    .iter()
                    .flat_map(|variant| variant.members.iter())
                    .map(|member| member.type_ref),
            );
        }
        _ => {}
    }
    edges
}

async fn load_fixture_image(fixture: &str) -> Arc<ModuleImage> {
    let debugger = Debugger::new(Scenario::fixture(fixture))
        .unwrap_or_else(|error| panic!("load {fixture} metadata: {error}"));
    let image = Arc::clone(debugger.handle().module_image());
    debugger
        .shutdown()
        .await
        .unwrap_or_else(|error| panic!("shut down {fixture} metadata session: {error}"));
    image
}

fn assert_inspected_signed(value: &uscope::InspectedValue, expected: i128, context: &str) {
    assert!(
        matches!(
            available_value(&value.state),
            uscope::VariableValue::Scalar(ScalarValue::Signed(actual)) if *actual == expected
        ),
        "{context}: {value:?}"
    );
}

fn single_image_breakpoint_address(breakpoint: &uscope::Breakpoint) -> uscope::ImageAddress {
    assert_eq!(breakpoint.locations.len(), 1);
    match breakpoint.locations[0].location {
        BreakpointLocation::Image(address) => address,
        BreakpointLocation::Virtual(_) => panic!("function breakpoint was not image-based"),
    }
}

async fn dereference_named(
    scenario: &Scenario,
    name: &str,
    depth: usize,
) -> uscope::DereferencedValue {
    let variable = scenario
        .operation("inspect pointer", scenario.handle().variable(name))
        .await;
    let mut state = variable.state;
    let mut result = None;
    for _ in 0..depth {
        let reference = match state {
            VariableState::Available {
                dereference: uscope::DereferenceState::Available(reference),
                ..
            } => reference,
            other => panic!("{name} is not dereferenceable: {other:?}"),
        };
        let value = scenario
            .operation(
                "dereference pointer",
                scenario.handle().dereference(reference),
            )
            .await;
        state = value.state.clone();
        result = Some(value);
    }
    result.expect("positive dereference depth")
}

fn assert_dereferenced_scalar(value: &uscope::DereferencedValue, expected: i128, fixture: &str) {
    assert!(
        matches!(
            available_value(&value.state),
            uscope::VariableValue::Scalar(ScalarValue::Signed(actual)) if *actual == expected
        ),
        "{fixture}: {value:?}"
    );
}

async fn assert_array_values(
    scenario: &Scenario,
    value: &uscope::DereferencedValue,
    fixture: &str,
) {
    let uscope::VariableValue::Array { .. } = available_value(&value.state) else {
        panic!("{fixture}: expected decoded array, got {value:?}");
    };
    let page = child_page(scenario, &value.state, 0, 2).await;
    let values: Vec<i128> = page
        .children
        .iter()
        .map(|child| match available_value(&child.state) {
            uscope::VariableValue::Scalar(uscope::ScalarValue::Signed(value)) => *value,
            other => panic!("{fixture}: expected scalar array element, got {other:?}"),
        })
        .collect();
    assert_eq!(values, [20, 22], "{fixture}");
}

async fn assert_slice_values(
    scenario: &Scenario,
    variable: &uscope::Variable,
    capacity: Option<u64>,
    expected: &[i128],
    fixture: &str,
) {
    let uscope::VariableValue::Slice {
        length,
        capacity: actual_capacity,
    } = available_value(&variable.state)
    else {
        panic!("{fixture}: expected decoded slice, got {variable:?}");
    };
    assert_eq!(*length, u64::try_from(expected.len()).unwrap(), "{fixture}");
    assert_eq!(*actual_capacity, capacity, "{fixture}");
    let page = if expected.is_empty() {
        None
    } else {
        Some(
            child_page(
                scenario,
                &variable.state,
                0,
                u32::try_from(expected.len()).expect("test slice length fits u32"),
            )
            .await,
        )
    };
    let values: Vec<i128> = page
        .as_ref()
        .map_or(&[][..], |page| page.children.as_ref())
        .iter()
        .map(|child| match available_value(&child.state) {
            uscope::VariableValue::Scalar(uscope::ScalarValue::Signed(value)) => *value,
            other => panic!("{fixture}: expected scalar slice element, got {other:?}"),
        })
        .collect();
    assert_eq!(values, expected, "{fixture}");
}

async fn launch_boundary_scenario(
    name: String,
    fixture: &str,
) -> (Scenario, uscope::ExecutionLocation) {
    let mut scenario = Scenario::new(name, Scenario::fixture(fixture));
    scenario.add_breakpoint("main").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let main = scenario
        .operation("main activation", scenario.handle().current_location())
        .await;
    (scenario, main)
}

async fn boundary_source_step(
    scenario: &mut Scenario,
    kind: StepKind,
    operation: &str,
) -> uscope::ExecutionLocation {
    assert_eq!(
        scenario.step_to_stop(kind).await,
        StopReason::Step { kind },
        "{operation}"
    );
    scenario
        .operation(operation, scenario.handle().current_location())
        .await
}

fn boundary_function(location: &uscope::ExecutionLocation) -> Option<&str> {
    location
        .image
        .function
        .as_ref()
        .map(|function| function.name.as_ref())
}

fn boundary_line(location: &uscope::ExecutionLocation) -> Option<u64> {
    location
        .image
        .source
        .as_ref()
        .map(|source| source.line.get())
}

fn fixture_symbol_address(
    scenario: &Scenario,
    location: &uscope::ExecutionLocation,
    symbol: &str,
) -> VirtualAddress {
    let address = scenario
        .handle()
        .module_image()
        .symbol_named(symbol)
        .unwrap_or_else(|error| panic!("missing fixture symbol {symbol}: {error}"))
        .address;
    relocate_image_address(address, location)
}

async fn boundary_sink_value(scenario: &Scenario, sink: VirtualAddress) -> u64 {
    scenario
        .operation("boundary sink", scenario.handle().read_word(sink))
        .await
        & u64::from(u32::MAX)
}

async fn advance_boundary_to_line(
    scenario: &mut Scenario,
    fixture: &str,
    target: u64,
) -> uscope::ExecutionLocation {
    for _ in 0..4 {
        let location = scenario
            .operation(
                "advance boundary call site",
                scenario.handle().current_location(),
            )
            .await;
        assert_eq!(boundary_function(&location), Some("main"));
        let line = boundary_line(&location).expect("main call site has source");
        if line == target {
            return location;
        }
        assert!(
            line < target,
            "{fixture} skipped target line {target} and stopped at {line}"
        );
        boundary_source_step(scenario, StepKind::OverSource, "advance boundary call site").await;
    }
    panic!("{fixture} did not reach main line {target} within the step budget");
}

async fn finish_boundary_physical_call(
    scenario: &mut Scenario,
    fixture: &str,
    main_physical: Option<uscope::CodeInstanceId>,
    sink: VirtualAddress,
    case: (u64, &str, u64, u64),
) {
    let (call_line, callee, expected_sink, last_caller_line) = case;
    advance_boundary_to_line(scenario, fixture, call_line).await;
    let entered =
        boundary_source_step(scenario, StepKind::IntoSource, "entered physical callee").await;
    assert_eq!(boundary_function(&entered), Some(callee));
    assert_ne!(entered.image.physical_instance, main_physical);

    let returned =
        boundary_source_step(scenario, StepKind::Out, "caller after physical finish").await;
    assert_eq!(boundary_function(&returned), Some("main"));
    assert_eq!(returned.image.physical_instance, main_physical);
    let line = boundary_line(&returned).expect("physical finish has caller source");
    assert!(
        (call_line..=last_caller_line).contains(&line),
        "{fixture} finished {callee} at unexpected line {line}"
    );
    assert_eq!(
        boundary_sink_value(scenario, sink).await,
        expected_sink,
        "{fixture} finished the wrong path through {callee}"
    );
}

/// Steps into a fixture function until the selected frame is the named
/// inline instance stopped at the requested source line.
async fn enter_inline_frame(scenario: &mut Scenario, fixture: &str, function: &str, line: u64) {
    for _ in 0..8 {
        let location = scenario
            .operation(
                "inline frame location",
                scenario.handle().current_location(),
            )
            .await;
        if boundary_function(&location) == Some(function) && boundary_line(&location) == Some(line)
        {
            return;
        }
        boundary_source_step(scenario, StepKind::IntoSource, "enter inline frame").await;
    }
    panic!("{fixture} did not reach {function}:{line} within the step budget");
}

async fn advance_to_boundary_inline_call(
    scenario: &mut Scenario,
    fixture: &str,
) -> uscope::ExecutionLocation {
    for _ in 0..2 {
        let location = scenario
            .operation("boundary inline call", scenario.handle().current_location())
            .await;
        let is_main_call = location
            .image
            .function
            .as_ref()
            .is_some_and(|function| function.name.as_ref() == "main")
            && location
                .image
                .source
                .as_ref()
                .is_some_and(|source| source.line.get() == 30);
        if is_main_call {
            return location;
        }
        assert_eq!(
            scenario.step_to_stop(StepKind::OverSource).await,
            StopReason::Step {
                kind: StepKind::OverSource
            },
            "{fixture}"
        );
    }
    panic!("{fixture} did not reach the inline_adjust call in main");
}

#[derive(Clone, Copy)]
struct EntryBoundaryCase {
    fixture: &'static str,
    function: &'static str,
    source: &'static str,
    call_line: u64,
    parameter: &'static str,
    expected_value: i64,
    gcc_fallback_line: Option<u64>,
}

const fn entry_boundary_cases() -> [EntryBoundaryCase; 6] {
    [
        EntryBoundaryCase {
            fixture: "variables-parameters-gcc-o0",
            function: "all_parameters",
            source: "variables-parameters.c",
            call_line: 66,
            parameter: "signed_int",
            expected_value: -1_234_567,
            gcc_fallback_line: Some(21),
        },
        EntryBoundaryCase {
            fixture: "variables-parameters-gcc-o2",
            function: "all_parameters",
            source: "variables-parameters.c",
            call_line: 66,
            parameter: "signed_int",
            expected_value: -1_234_567,
            // This optimized leaf has no prologue. Its first instruction is
            // the line-22 store, so advancing to the next distinct statement
            // would silently execute real user work.
            gcc_fallback_line: Some(20),
        },
        EntryBoundaryCase {
            fixture: "variables-parameters-clang-o0",
            function: "all_parameters",
            source: "variables-parameters.c",
            call_line: 66,
            parameter: "signed_int",
            expected_value: -1_234_567,
            gcc_fallback_line: None,
        },
        EntryBoundaryCase {
            fixture: "variables-parameters-clang-o2",
            function: "all_parameters",
            source: "variables-parameters.c",
            call_line: 66,
            parameter: "signed_int",
            expected_value: -1_234_567,
            gcc_fallback_line: None,
        },
        EntryBoundaryCase {
            fixture: "variables-rust-o0",
            function: "inspect_scalars",
            source: "variables.rs",
            call_line: 93,
            parameter: "signed_value",
            expected_value: -42,
            gcc_fallback_line: None,
        },
        EntryBoundaryCase {
            fixture: "variables-rust-o2",
            function: "inspect_scalars",
            source: "variables.rs",
            call_line: 93,
            parameter: "signed_value",
            expected_value: -42,
            gcc_fallback_line: None,
        },
    ]
}

fn expected_physical_entry(scenario: &Scenario, case: &EntryBoundaryCase) -> uscope::ImageAddress {
    let image = scenario.handle().module_image();
    let function = image
        .function_named(case.function)
        .unwrap_or_else(|error| panic!("{} missing {}: {error}", case.fixture, case.function));
    let instance = image
        .instances_for_function(function.id)
        .find(|instance| matches!(instance.kind, CodeInstanceKind::OutOfLine))
        .unwrap_or_else(|| panic!("{} missing physical {}", case.fixture, case.function));

    if let Some(marker) = image
        .statement_rows()
        .iter()
        .find(|row| row.flags.prologue_end() && instance.contains(row.address))
    {
        return marker.address;
    }

    let line = case
        .gcc_fallback_line
        .expect("markerless entry case has an explicit conservative expectation");
    image
        .statement_rows()
        .iter()
        .find(|row| {
            instance.contains(row.address)
                && row.flags.is_statement()
                && row
                    .location
                    .as_ref()
                    .is_some_and(|location| location.line.get() == line)
        })
        .map_or_else(
            || {
                panic!(
                    "{} missing expected markerless entry line {line}",
                    case.fixture
                )
            },
            |row| row.address,
        )
}

async fn assert_entry_stop(
    scenario: &Scenario,
    case: &EntryBoundaryCase,
    expected: uscope::ImageAddress,
) {
    let location = scenario
        .operation(
            "physical entry location",
            scenario.handle().current_location(),
        )
        .await;
    assert_eq!(location.image.address, expected, "{}", case.fixture);
    assert_eq!(
        location
            .image
            .function
            .as_ref()
            .map(|function| function.name.as_ref()),
        Some(case.function),
        "{}",
        case.fixture
    );
    let parameter = scenario
        .operation(
            "entry parameter value",
            scenario.handle().variable(case.parameter),
        )
        .await;
    assert_variable_value(
        &parameter,
        ScalarValue::Signed(i128::from(case.expected_value)),
    );
}

fn epilogue_markers(scenario: &Scenario, function: &str) -> BTreeSet<uscope::ImageAddress> {
    let image = scenario.handle().module_image();
    let function = image.function_named(function).expect("marked function");
    let instance = image
        .instances_for_function(function.id)
        .find(|instance| matches!(instance.kind, CodeInstanceKind::OutOfLine))
        .expect("physical marked function");
    image
        .statement_rows()
        .iter()
        .filter(|row| row.flags.epilogue_begin() && instance.contains(row.address))
        .map(|row| row.address)
        .collect()
}

const fn relocate_image_address(
    address: uscope::ImageAddress,
    location: &uscope::ExecutionLocation,
) -> VirtualAddress {
    let load_bias = location
        .address
        .get()
        .checked_sub(location.image.address.get())
        .expect("runtime address includes the image load bias");
    VirtualAddress::new(
        load_bias
            .checked_add(address.get())
            .expect("relocated test address fits u64"),
    )
}

fn catalog_global<'a>(
    image: &'a ModuleImage,
    qualified_name: &str,
) -> &'a uscope::GlobalVariableInfo {
    image
        .globals()
        .iter()
        .find(|global| global.qualified_name.as_ref() == qualified_name)
        .unwrap_or_else(|| {
            panic!(
                "missing global {qualified_name}; catalog: {:?}",
                image
                    .globals()
                    .iter()
                    .map(|global| global.qualified_name.as_ref())
                    .collect::<Vec<_>>()
            )
        })
}

async fn assert_go_pointer_values(scenario: &Scenario, fixture: &str) {
    for (name, depth) in [("pointerParameter", 1), ("pointerPointer", 2)] {
        assert_dereferenced_scalar(&dereference_named(scenario, name, depth).await, 42, fixture);
    }
    let nil_pointer = scenario
        .operation(
            "inspect Go nil pointer",
            scenario.handle().variable("nilPointer"),
        )
        .await;
    assert!(
        matches!(
            nil_pointer.state,
            VariableState::Available {
                dereference: uscope::DereferenceState::Unavailable {
                    reason: uscope::DereferenceUnavailableReason::Null,
                    ..
                },
                ..
            }
        ),
        "{nil_pointer:?}"
    );
    let structure = dereference_named(scenario, "structurePointer", 1).await;
    assert_dereferenced_record(scenario, &structure, 2, fixture).await;
    let recursive = dereference_named(scenario, "recursivePointer", 1).await;
    assert_dereferenced_record(scenario, &recursive, 2, fixture).await;
    let slice = scenario
        .operation("inspect Go slice", scenario.handle().variable("sliceValue"))
        .await;
    assert_slice_values(scenario, &slice, Some(2), &[20, 22], fixture).await;
}

async fn run_go_to_breakpoint(scenario: &mut Scenario, fixture: &str) {
    let mut reason = scenario.run_to_stop().await;
    for _ in 0..32 {
        match reason {
            StopReason::Breakpoint { .. } => return,
            StopReason::Exception(ref exception) if exception.code == 23 => {
                reason = scenario.resume_to_stop().await;
            }
            _ => panic!("{fixture} stopped unexpectedly before its user breakpoint: {reason:?}"),
        }
    }
    panic!("{fixture} did not reach its user breakpoint after 32 runtime signals");
}

async fn resume_go_to_exit(scenario: &mut Scenario, fixture: &str) {
    let mut reason = scenario.resume_to_stop().await;
    for _ in 0..32 {
        match reason {
            StopReason::Exited(ExitStatus::Code(0)) => return,
            StopReason::Breakpoint { .. } => {
                reason = scenario.resume_to_stop().await;
            }
            // The runtime's SIGURG preemption never stops the program.
            _ => panic!("{fixture} stopped unexpectedly while exiting: {reason:?}"),
        }
    }
    panic!("{fixture} did not exit after 32 breakpoint stops");
}

async fn step_to_source_line(scenario: &mut Scenario, line: u32) {
    for _ in 0..16 {
        let reason = scenario.step_to_stop(StepKind::IntoSource).await;
        assert!(
            matches!(reason, StopReason::Step { .. }),
            "source step terminated unexpectedly: {reason:?}"
        );
        let location = scenario
            .operation("step location", scenario.handle().current_location())
            .await;
        if location
            .image
            .source
            .as_ref()
            .is_some_and(|source| source.line.get() == u64::from(line))
        {
            return;
        }
    }
    panic!("did not reach source line {line} within the step budget");
}

fn assert_variable_value(variable: &uscope::Variable, expected: impl Into<ScalarValue>) {
    assert_eq!(
        available_value(&variable.state),
        &uscope::VariableValue::Scalar(expected.into())
    );
}

fn assert_all_parameter_values(snapshot: &uscope::VariableSnapshot, fixture: &str) {
    assert_parameter_catalog(snapshot, fixture);
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
        ScalarValue::Signed(99),
    ];
    let expected_sizes = [1, 1, 1, 1, 2, 2, 4, 4, 8, 8, 8, 8, 4, 8, 16, 4];
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
        assert!(matches!(
            source,
            uscope::VariableValueSource::Memory(address) if address.get() != 0
        ));
        assert_eq!(
            raw.as_ref().expect("available scalar bytes").len(),
            usize::try_from(expected_size).unwrap()
        );
    }
}

fn assert_parameter_catalog(snapshot: &uscope::VariableSnapshot, fixture: &str) {
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
            "local",
        ],
        "{fixture}"
    );
    assert!(
        snapshot.variables[..15]
            .iter()
            .all(|variable| variable.kind == VariableKind::Parameter)
    );
    assert_eq!(snapshot.variables[15].kind, VariableKind::Local);
}

fn assert_optimized_parameter_values(snapshot: &uscope::VariableSnapshot, fixture: &str) {
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
    ];
    match fixture {
        "variables-parameters-gcc-o2" => {
            for (variable, value) in snapshot.variables[..14].iter().zip(&expected) {
                assert_variable_value(variable, value.clone());
            }
            for (index, register) in [
                (0, "rdi"),
                (1, "rsi"),
                (2, "rdx"),
                (3, "rcx"),
                (4, "r8"),
                (5, "r9"),
                (12, "xmm0"),
                (13, "xmm1"),
            ] {
                assert_register_source(&snapshot.variables[index], register, fixture);
            }
            for variable in &snapshot.variables[6..12] {
                assert_memory_source(variable, fixture);
            }
            assert_variable_value(
                &snapshot.variables[14],
                ScalarValue::Floating(uscope::FloatValue::X87Extended {
                    significand: 0xc800_0000_0000_0000,
                    sign_exponent: 0x4000,
                }),
            );
            assert_memory_source(&snapshot.variables[14], fixture);
        }
        "variables-parameters-clang-o2" => {
            for (index, value) in [0, 4, 5]
                .into_iter()
                .chain(6..14)
                .map(|index| (index, expected[index].clone()))
            {
                assert_variable_value(&snapshot.variables[index], value);
            }
            assert!(
                matches!(
                    snapshot.variables[0].state,
                    VariableState::Available {
                        source: uscope::VariableValueSource::Computed,
                        ..
                    }
                ),
                "{fixture}: {:?}",
                snapshot.variables[0]
            );
            for index in 1..4 {
                assert_unsupported(
                    &snapshot.variables[index],
                    uscope::UnsupportedVariableFeature::EntryValue,
                    fixture,
                );
            }
            assert_register_source(&snapshot.variables[4], "r8", fixture);
            assert_register_source(&snapshot.variables[5], "r9", fixture);
            for variable in &snapshot.variables[6..12] {
                assert_memory_source(variable, fixture);
            }
            assert_register_source(&snapshot.variables[12], "xmm0", fixture);
            assert_register_source(&snapshot.variables[13], "xmm1", fixture);
            assert_unsupported(
                &snapshot.variables[14],
                uscope::UnsupportedVariableFeature::CompositeLocation,
                fixture,
            );
        }
        _ => panic!("unexpected optimized parameter fixture {fixture}"),
    }
    assert_variable_value(&snapshot.variables[15], ScalarValue::Signed(99));
    assert!(
        matches!(
            snapshot.variables[15].state,
            VariableState::Available {
                source: uscope::VariableValueSource::Constant,
                ..
            }
        ),
        "{fixture}: {:?}",
        snapshot.variables[15]
    );
}

fn assert_register_source(variable: &uscope::Variable, name: &str, fixture: &str) {
    assert!(
        matches!(&variable.state, VariableState::Available {
        source: uscope::VariableValueSource::Register(register),
        ..
    } if register.name.as_ref() == name),
        "{fixture}: {variable:?}"
    );
}

fn assert_memory_source(variable: &uscope::Variable, fixture: &str) {
    assert!(
        matches!(variable.state, VariableState::Available {
        source: uscope::VariableValueSource::Memory(address),
        ..
    } if address.get() != 0),
        "{fixture}: {variable:?}"
    );
}

fn assert_unsupported(
    variable: &uscope::Variable,
    feature: uscope::UnsupportedVariableFeature,
    fixture: &str,
) {
    assert_eq!(
        variable.state,
        VariableState::Unavailable(uscope::VariableUnavailableReason::Unsupported(feature)),
        "{fixture}: {variable:?}"
    );
}

fn assert_language_scalar_values(snapshot: &uscope::VariableSnapshot, fixture: &str) {
    assert_language_scalar_catalog(snapshot, fixture);
    let expected = language_scalar_values();
    let sizes = [1, 4, 8, 4, 8, 1, 4, 8, 4, 8];
    for ((variable, expected), size) in snapshot.variables.iter().zip(expected).zip(sizes) {
        assert_variable_value(variable, expected);
        assert_eq!(
            variable
                .type_info
                .as_ref()
                .expect("available language scalar type")
                .byte_size,
            Some(size),
            "{fixture}: {variable:?}"
        );
        let VariableState::Available { source, raw, .. } = &variable.state else {
            unreachable!("value assertion checked availability")
        };
        assert!(matches!(source, uscope::VariableValueSource::Memory(_)));
        assert_eq!(
            raw.as_ref().expect("available scalar bytes").len(),
            usize::try_from(size).unwrap()
        );
    }
}

const fn language_scalar_values() -> [ScalarValue; 10] {
    [
        ScalarValue::Boolean(true),
        ScalarValue::Signed(-42),
        ScalarValue::Unsigned(42),
        ScalarValue::Floating(uscope::FloatValue::Binary32(1.25_f32.to_bits())),
        ScalarValue::Floating(uscope::FloatValue::Binary64((-2.5_f64).to_bits())),
        ScalarValue::Boolean(false),
        ScalarValue::Signed(-41),
        ScalarValue::Unsigned(44),
        ScalarValue::Floating(uscope::FloatValue::Binary32(1.75_f32.to_bits())),
        ScalarValue::Floating(uscope::FloatValue::Binary64((-2.75_f64).to_bits())),
    ]
}

fn assert_optimized_language_scalar_values(snapshot: &uscope::VariableSnapshot, fixture: &str) {
    let expected = language_scalar_values();
    let available = match fixture {
        "variables-cpp-gcc-o2" => (0..10).collect::<Vec<_>>(),
        "variables-cpp-clang-o2" => vec![0, 2, 3, 4, 5, 6, 7],
        "variables-rust-o2" => vec![0, 1, 2, 3, 4, 5, 6, 7],
        _ => panic!("unexpected optimized language fixture {fixture}"),
    };
    for index in available {
        assert_variable_value(&snapshot.variables[index], expected[index].clone());
    }
    if fixture == "variables-cpp-clang-o2" {
        assert_unsupported(
            &snapshot.variables[1],
            uscope::UnsupportedVariableFeature::EntryValue,
            fixture,
        );
    }
    if fixture != "variables-cpp-gcc-o2" {
        for index in 8..10 {
            assert_eq!(
                snapshot.variables[index].state,
                VariableState::Unavailable(
                    uscope::VariableUnavailableReason::UnavailableAtInstruction
                ),
                "{fixture}: {:?}",
                snapshot.variables[index]
            );
        }
    }
}

fn assert_language_scalar_catalog(snapshot: &uscope::VariableSnapshot, fixture: &str) {
    let names = snapshot
        .variables
        .iter()
        .map(|variable| variable.name.as_ref())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "flag",
            "signed_value",
            "unsigned_value",
            "single",
            "double_precision",
            "local_flag",
            "local_signed",
            "local_unsigned",
            "local_single",
            "local_double",
        ],
        "{fixture}"
    );
    assert!(
        snapshot.variables[..5]
            .iter()
            .all(|variable| variable.kind == VariableKind::Parameter)
    );
    assert!(
        snapshot.variables[5..]
            .iter()
            .all(|variable| variable.kind == VariableKind::Local)
    );
}

fn register_u64(registers: &uscope::RegisterSnapshot, role: RegisterRole) -> u64 {
    let value = registers
        .registers
        .iter()
        .find(|value| value.register.role == Some(role))
        .unwrap_or_else(|| panic!("missing {role:?} register"));
    let bytes: [u8; 8] = value
        .bytes
        .as_deref()
        .unwrap_or_else(|| panic!("{} was not saved", value.register.name))
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
    assert_eq!(context.location.line.get(), 6);
    assert_eq!(current.text.as_ref(), "    return uscope_value;");
    assert_eq!(
        context
            .lines
            .first()
            .expect("first source line")
            .number
            .get(),
        3
    );
    assert_eq!(
        context.lines.last().expect("last source line").number.get(),
        9
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
        value.register.name.as_ref() == "rax"
            && value.register.bits == 64
            && value.bytes.as_ref().is_some_and(|bytes| bytes.len() == 8)
    }));
}

/// Summarizes one backtrace frame as its owning module's file name and its
/// function name, so cross-module assertions name both identities.
fn frame_modules(
    trace: &uscope::Backtrace,
    modules: &uscope::LoadedModuleSnapshot,
) -> Vec<(String, Option<String>)> {
    trace
        .frames
        .iter()
        .map(|frame| {
            let module = frame.module.map_or_else(
                || "?".to_owned(),
                |id| {
                    let record = modules
                        .modules
                        .iter()
                        .find(|record| record.module.id == id)
                        .unwrap_or_else(|| panic!("frame references unknown module {id:?}"));
                    record
                        .path
                        .file_name()
                        .expect("module path names a file")
                        .to_string_lossy()
                        .into_owned()
                },
            );
            (
                module,
                frame
                    .function
                    .as_ref()
                    .map(|function| function.name.to_string()),
            )
        })
        .collect()
}

fn position_of(frames: &[(String, Option<String>)], module: &str, function: &str) -> usize {
    frames
        .iter()
        .position(|(frame_module, name)| {
            frame_module == module && name.as_deref() == Some(function)
        })
        .unwrap_or_else(|| panic!("no {module}:{function} frame in {frames:#?}"))
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
