mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use uscope::{
    Architecture, BreakpointLocation, ByteOrder, CodeInstanceKind, Debugger, EntryProvenance,
    Error, ExitStatus, InferiorState, InlineFrameLookup, ModuleImage, PointerWidth, RegisterRole,
    ScalarValue, SourceContext, SourceFile, SourceLocation, StepKind, StopReason, ThreadState,
    UnwindTermination, VariableKind, VariableState, VariableUnavailableReason, VirtualAddress,
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
            completion: uscope::ValuePageCompletion::Complete,
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

fn value_expression(components: &[&str]) -> uscope::ValueExpression {
    uscope::ValueExpression {
        steps: components
            .iter()
            .map(|component| uscope::ValuePathStep::Named((*component).to_owned()))
            .collect::<Vec<_>>()
            .into(),
    }
}

fn parsed_value_expression(expression: &str) -> uscope::ValueExpression {
    let parsed = uscope::parse_value_expression(expression)
        .unwrap_or_else(|error| panic!("parse test value expression {expression:?}: {error}"));
    assert_eq!(
        parsed.range, None,
        "test expression unexpectedly selected a range"
    );
    parsed.expression
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

#[tokio::test]
async fn normalized_type_graph_is_public_dense_and_closed_across_languages() {
    for fixture in [
        "variables-gcc-o0",
        "variables-clang-o0",
        "types-c-gcc-o0",
        "types-c-clang-o0",
        "variables-cpp-gcc-o0",
        "variables-cpp-clang-o0",
        "variables-rust-o0",
        "variables-zig-o0",
        "variables-go-o0",
        "types-cpp-gcc-dwarf4",
        "types-cpp-gcc-dwarf5",
    ] {
        let image = load_fixture_image(fixture).await;
        assert!(!image.types().is_empty(), "{fixture}");
        for (index, node) in image.types().iter().enumerate() {
            let reference = node.reference();
            assert_eq!(reference.image, image.id(), "{fixture}: {node:?}");
            assert_eq!(
                usize::try_from(reference.id.get()).expect("type ID fits usize"),
                index,
                "{fixture}: {node:?}"
            );
            assert_eq!(
                image.type_node(reference),
                Some(node),
                "{fixture}: {node:?}"
            );
            if let uscope::TypeNode::Resolved(info) = node {
                assert!(
                    !info.name.contains("<recursive type>"),
                    "{fixture}: construction placeholder escaped into finalized type metadata: {info:?}"
                );
                for edge in type_edges(&info.kind) {
                    assert!(
                        image.type_node(edge).is_some(),
                        "{fixture}: dangling edge {edge:?} from {info:?}"
                    );
                }
            }
        }
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one compiler-language matrix keeps the cross-language semantic contract visible"
)]
async fn normalized_named_types_and_modifiers_preserve_language_semantics() {
    for fixture in ["variables-gcc-o0", "variables-clang-o0"] {
        let image = load_fixture_image(fixture).await;
        let resolved = image
            .types()
            .iter()
            .filter_map(|node| match node {
                uscope::TypeNode::Resolved(info) => Some(info),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            resolved.iter().any(|info| {
                info.name.as_ref() == "aliased_int"
                    && matches!(
                        info.kind,
                        uscope::TypeKind::Named {
                            target: Some(_),
                            relationship: uscope::NamedTypeRelationship::Synonym,
                        }
                    )
            }),
            "{fixture}: {resolved:#?}"
        );
        assert!(
            resolved.iter().any(|info| {
                info.name.as_ref() == "const int *"
                    && matches!(info.kind, uscope::TypeKind::Pointer { .. })
            }),
            "{fixture}: {resolved:#?}"
        );
        assert!(
            resolved.iter().any(|info| {
                info.name.as_ref() == "int * const"
                    && matches!(
                        info.kind,
                        uscope::TypeKind::Modified {
                            modifier: uscope::TypeModifier::Const,
                            ..
                        }
                    )
            }),
            "{fixture}: {resolved:#?}"
        );
    }

    for fixture in ["variables-cpp-gcc-o0", "variables-cpp-clang-o0"] {
        let image = load_fixture_image(fixture).await;
        assert!(
            image.types().iter().any(|node| matches!(
                node,
                uscope::TypeNode::Resolved(uscope::TypeInfo {
                    name,
                    kind: uscope::TypeKind::Named {
                        target: Some(_),
                        relationship: uscope::NamedTypeRelationship::Synonym,
                    },
                    ..
                }) if name.as_ref() == "aliased_int"
            )),
            "{fixture}: C++ alias was not normalized as a synonym"
        );
    }

    for fixture in ["types-c-gcc-o0", "types-c-clang-o0"] {
        let image = load_fixture_image(fixture).await;
        let modifiers = image
            .types()
            .iter()
            .filter_map(|node| match node {
                uscope::TypeNode::Resolved(uscope::TypeInfo {
                    kind: uscope::TypeKind::Modified { modifier, .. },
                    ..
                }) => Some(*modifier),
                _ => None,
            })
            .collect::<Vec<_>>();
        for expected in [
            uscope::TypeModifier::Const,
            uscope::TypeModifier::Volatile,
            uscope::TypeModifier::Restrict,
            uscope::TypeModifier::Atomic,
        ] {
            assert!(
                modifiers.contains(&expected),
                "{fixture}: missing {expected:?} in {modifiers:?}"
            );
        }
    }

    let go = load_fixture_image("variables-go-o0").await;
    assert!(
        go.types().iter().any(|node| matches!(
            node,
            uscope::TypeNode::Resolved(uscope::TypeInfo {
                kind: uscope::TypeKind::Named {
                    relationship: uscope::NamedTypeRelationship::Distinct,
                    target: Some(_),
                },
                ..
            })
        )),
        "Go definitions must retain distinct identity"
    );
    let go_names = go
        .types()
        .iter()
        .filter_map(|node| match node {
            uscope::TypeNode::Resolved(info) => Some(info.name.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        go_names.contains(&"main.definedInt"),
        "Go defined scalar type was lost: {go_names:?}"
    );
    assert!(
        go_names
            .iter()
            .any(|name| name.contains("recursiveList[int32]")),
        "Go instantiated recursive generic type was lost: {go_names:?}"
    );
    assert!(
        !go_names.iter().any(|name| name.contains("scalarAlias")),
        "Go source aliases erased by the producer must not be reconstructed: {go_names:?}"
    );
    drop(go);

    let zig = load_fixture_image("variables-zig-o0").await;
    assert!(
        zig.types().iter().any(|node| matches!(
            node,
            uscope::TypeNode::Resolved(uscope::TypeInfo {
                kind: uscope::TypeKind::Named {
                    relationship: uscope::NamedTypeRelationship::Encoding,
                    target: Some(_),
                },
                ..
            })
        )),
        "Zig producer wrappers must be identified as encodings"
    );
    drop(zig);

    let rust = load_fixture_image("variables-rust-o0").await;
    assert!(
        rust.types()
            .iter()
            .filter_map(|node| match node {
                uscope::TypeNode::Resolved(info) => Some(info.name.as_ref()),
                _ => None,
            })
            .all(|name| name != "AliasedInt"),
        "rustc-erased source aliases must not be reconstructed heuristically"
    );
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

#[tokio::test]
async fn stack_scalar_variables_are_read_through_the_public_scenario_path() {
    for fixture in ["variables-gcc-o0", "variables-clang-o0"] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
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
async fn variable_inspection_uses_live_values_and_lexical_scope() {
    let mut changing = Scenario::new(
        "changing stack variable",
        Scenario::fixture("variables-gcc-o0"),
    );
    changing.add_source_breakpoint("variables.c", 30).await;
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
    shadow.add_source_breakpoint("variables.c", 39).await;
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
async fn pointer_variables_are_available_and_explicitly_dereferenceable() {
    for fixture in ["variables-gcc-o0", "variables-clang-o0"] {
        let mut partial = Scenario::new(fixture, Scenario::fixture(fixture));
        partial.add_source_breakpoint("variables.c", 48).await;
        assert!(matches!(
            partial.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let variables = partial
            .operation("partial variables", partial.handle().variables())
            .await;
        assert_eq!(variables.variables.len(), 2);
        assert_variable_value(&variables.variables[0], ScalarValue::Signed(42));
        let pointer = &variables.variables[1];
        assert!(matches!(
            pointer.type_info.as_ref().map(|info| &info.kind),
            Some(uscope::TypeKind::Pointer { .. })
        ));
        let reference = match &pointer.state {
            VariableState::Available {
                value,
                dereference: uscope::DereferenceState::Available(reference),
                ..
            } => {
                let uscope::VariableValue::Address(value) = value else {
                    panic!("pointer value was not an address: {value:?}");
                };
                assert_ne!(value.address.get(), 0);
                reference.clone()
            }
            state => panic!("pointer was not available for dereference: {state:?}"),
        };
        let dereferenced = partial
            .operation(
                "dereference pointer",
                partial.handle().dereference(reference.clone()),
            )
            .await;
        assert!(matches!(
            dereferenced.state,
            VariableState::Available {
                dereference: uscope::DereferenceState::NotApplicable,
                ..
            }
        ));
        assert_eq!(
            available_value(&dereferenced.state),
            &uscope::VariableValue::Scalar(ScalarValue::Signed(42))
        );
        let constrained = partial
            .operation(
                "bound scalar dereference bytes",
                partial.handle().dereference_with_limits(
                    reference,
                    uscope::InspectionLimits {
                        memory_bytes: 3,
                        ..uscope::InspectionLimits::default()
                    },
                ),
            )
            .await;
        assert!(matches!(
            constrained.state,
            VariableState::Unavailable(VariableUnavailableReason::InspectionLimit(
                uscope::InspectionExhaustion {
                    resource: uscope::InspectionLimit::MemoryBytes,
                    limit: 3,
                    used: 0,
                    requested: 4,
                }
            ))
        ));
        assert!(matches!(
            constrained.completion,
            uscope::InspectionCompletion::Truncated(uscope::InspectionExhaustion {
                resource: uscope::InspectionLimit::MemoryBytes,
                limit: 3,
                used: 0,
                requested: 4,
            })
        ));
        partial.shutdown().await;
    }
}

#[tokio::test]
async fn structural_inspection_selects_direct_and_pointer_record_members() {
    for fixture in ["variables-gcc-o0", "variables-clang-o0"] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_source_breakpoint("variables.c", 68).await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        for (components, expected) in [
            (&["pair", "first"][..], 20),
            (&["pair", "second"][..], 22),
            (&["structure_pointer", "first"][..], 20),
            (&["structure_pointer", "second"][..], 22),
        ] {
            let value = scenario
                .operation(
                    "inspect record member",
                    scenario.handle().inspect(value_expression(components)),
                )
                .await;
            assert_inspected_signed(&value, expected, fixture);
        }

        scenario.shutdown().await;
    }

    for fixture in ["variables-cpp-gcc-o0", "variables-cpp-clang-o0"] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_source_breakpoint("variables.cpp", 50).await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let value = scenario
            .operation(
                "inspect member through C++ reference",
                scenario
                    .handle()
                    .inspect(value_expression(&["structure_reference", "second"])),
            )
            .await;
        assert_inspected_signed(&value, 22, fixture);
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn structural_inspection_dereferences_each_intermediate_pointer_only_when_needed() {
    for fixture in [
        "variables-gcc-o0",
        "variables-clang-o0",
        "variables-gcc-o2",
        "variables-clang-o2",
    ] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_source_breakpoint("variables.c", 68).await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let terminal_pointer = scenario
            .operation(
                "inspect terminal pointer member",
                scenario
                    .handle()
                    .inspect(value_expression(&["recursive_pointer", "next"])),
            )
            .await;
        assert!(
            matches!(
                terminal_pointer.type_info.as_ref().map(|info| &info.kind),
                Some(uscope::TypeKind::Pointer { .. })
            ) && matches!(
                available_value(&terminal_pointer.state),
                uscope::VariableValue::Address(uscope::AddressValue { address })
                    if address.get() != 0
            ),
            "{fixture}: terminal pointer was implicitly dereferenced: {terminal_pointer:?}"
        );

        for (components, expected) in [
            (&["recursive_pointer", "value"][..], 40),
            (&["recursive_pointer", "next", "value"][..], 41),
            (&["recursive_pointer", "next", "next", "value"][..], 42),
        ] {
            let value = scenario
                .operation(
                    "inspect pointer member chain",
                    scenario.handle().inspect(value_expression(components)),
                )
                .await;
            assert_inspected_signed(&value, expected, fixture);
        }

        let unavailable = scenario
            .operation(
                "inspect through null intermediate pointer",
                scenario.handle().inspect(value_expression(&[
                    "recursive_pointer",
                    "next",
                    "next",
                    "next",
                    "value",
                ])),
            )
            .await;
        assert_eq!(
            unavailable
                .type_info
                .as_ref()
                .map(|type_info| type_info.name.as_ref()),
            Some("int"),
            "{fixture}: terminal type was lost after the null hop: {unavailable:?}"
        );
        assert!(
            matches!(unavailable.state, VariableState::Unavailable(_)),
            "{fixture}: {unavailable:?}"
        );

        scenario.shutdown().await;
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one table-driven scenario keeps the cross-language pointer contract identical"
)]
async fn thin_pointers_and_references_dereference_across_the_language_matrix() {
    for (fixture, source, line, pointer, nested, null) in [
        (
            "variables-gcc-o0",
            "variables.c",
            68,
            "pointer",
            "pointer_pointer",
            "null_pointer",
        ),
        (
            "variables-clang-o0",
            "variables.c",
            68,
            "pointer",
            "pointer_pointer",
            "null_pointer",
        ),
        (
            "variables-gcc-o2",
            "variables.c",
            68,
            "pointer",
            "pointer_pointer",
            "null_pointer",
        ),
        (
            "variables-clang-o2",
            "variables.c",
            68,
            "pointer",
            "pointer_pointer",
            "null_pointer",
        ),
        (
            "variables-gcc-nopie",
            "variables.c",
            68,
            "pointer",
            "pointer_pointer",
            "null_pointer",
        ),
        (
            "variables-cpp-gcc-o0",
            "variables.cpp",
            50,
            "lvalue_reference",
            "pointer_pointer",
            "null_pointer",
        ),
        (
            "variables-cpp-clang-o0",
            "variables.cpp",
            50,
            "lvalue_reference",
            "pointer_pointer",
            "null_pointer",
        ),
        (
            "variables-cpp-gcc-o2",
            "variables.cpp",
            50,
            "lvalue_reference",
            "pointer_pointer",
            "null_pointer",
        ),
        (
            "variables-cpp-clang-o2",
            "variables.cpp",
            50,
            "lvalue_reference",
            "pointer_pointer",
            "null_pointer",
        ),
        (
            "variables-rust-o0",
            "variables.rs",
            67,
            "shared",
            "raw_pointer",
            "null_pointer",
        ),
        (
            "variables-rust-o2",
            "variables.rs",
            80,
            "shared",
            "raw_pointer",
            "null_pointer",
        ),
        (
            "variables-zig-o0",
            "variables.zig",
            73,
            "pointer",
            "pointer_pointer",
            "null_pointer",
        ),
        (
            "variables-zig-o2",
            "variables.zig",
            85,
            "pointer",
            "pointer_pointer",
            "null_pointer",
        ),
        (
            "variables-zig-nopie",
            "variables.zig",
            73,
            "pointer",
            "pointer_pointer",
            "null_pointer",
        ),
    ] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_source_breakpoint(source, line).await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let pointee = dereference_named(&scenario, pointer, 1).await;
        assert_dereferenced_scalar(&pointee, 42, fixture);
        let nested_pointee = dereference_named(&scenario, nested, 2).await;
        assert_dereferenced_scalar(&nested_pointee, 42, fixture);

        let null = scenario
            .operation("inspect null pointer", scenario.handle().variable(null))
            .await;
        assert!(
            matches!(
                null.state,
                VariableState::Available {
                    dereference: uscope::DereferenceState::Unavailable {
                        reason: uscope::DereferenceUnavailableReason::Null,
                        ..
                    },
                    ..
                }
            ) && matches!(
                available_value(&null.state),
                uscope::VariableValue::Address(uscope::AddressValue { address })
                    if address.get() == 0
            ),
            "{fixture}: {null:?}"
        );
        if source == "variables.c" {
            assert_dereferenced_scalar(
                &dereference_named(&scenario, "pointer_parameter", 1).await,
                42,
                fixture,
            );
            assert_dereferenced_scalar(
                &dereference_named(&scenario, "alias_pointer", 1).await,
                42,
                fixture,
            );
            assert_dereferenced_scalar(
                &dereference_named(&scenario, "const_pointee", 1).await,
                42,
                fixture,
            );
            assert_dereferenced_scalar(
                &dereference_named(&scenario, "const_pointer", 1).await,
                42,
                fixture,
            );
            let void_pointer = scenario
                .operation(
                    "inspect void pointer",
                    scenario.handle().variable("void_pointer"),
                )
                .await;
            assert!(
                matches!(
                    void_pointer.state,
                    VariableState::Available {
                        dereference: uscope::DereferenceState::Unavailable {
                            reason: uscope::DereferenceUnavailableReason::UnspecifiedPointee,
                            ..
                        },
                        ..
                    }
                ),
                "{fixture}: {void_pointer:?}"
            );
            let invalid = dereference_named(&scenario, "invalid_pointer", 1).await;
            assert!(matches!(invalid.state, VariableState::Unavailable(_)));
        }
        if source == "variables.cpp" {
            assert_dereferenced_scalar(
                &dereference_named(&scenario, "pointer_parameter", 1).await,
                42,
                fixture,
            );
            assert_dereferenced_scalar(
                &dereference_named(&scenario, "reference_parameter", 1).await,
                42,
                fixture,
            );
            assert_dereferenced_scalar(
                &dereference_named(&scenario, "const_reference", 1).await,
                42,
                fixture,
            );
            assert_dereferenced_scalar(
                &dereference_named(&scenario, "alias_pointer", 1).await,
                42,
                fixture,
            );
            for (name, kind) in [
                ("lvalue_reference", uscope::ReferenceKind::Lvalue),
                ("rvalue_reference", uscope::ReferenceKind::Rvalue),
            ] {
                let variable = scenario
                    .operation(
                        "inspect C++ reference kind",
                        scenario.handle().variable(name),
                    )
                    .await;
                assert!(
                    matches!(
                        variable.type_info.as_ref().map(|info| &info.kind),
                        Some(uscope::TypeKind::Reference { kind: actual, .. }) if *actual == kind
                    ),
                    "{fixture}: {variable:?}"
                );
            }
            assert_dereferenced_scalar(
                &dereference_named(&scenario, "rvalue_reference", 1).await,
                42,
                fixture,
            );
            assert_dereferenced_scalar(
                &dereference_named(&scenario, "reference_to_pointer", 2).await,
                42,
                fixture,
            );
        }
        if source == "variables.rs" {
            for name in [
                "shared_parameter",
                "raw_parameter",
                "raw_const",
                "alias_pointer",
            ] {
                assert_dereferenced_scalar(
                    &dereference_named(&scenario, name, 1).await,
                    42,
                    fixture,
                );
            }
            let slice = scenario
                .operation("inspect Rust slice", scenario.handle().variable("slice"))
                .await;
            if fixture == "variables-rust-o0" {
                assert_slice_values(&scenario, &slice, None, &[20, 22], fixture).await;
                let indexed = scenario
                    .operation(
                        "inspect one Rust slice element directly",
                        scenario
                            .handle()
                            .inspect(parsed_value_expression("slice[1]")),
                    )
                    .await;
                assert_inspected_signed(&indexed, 22, fixture);
                let range =
                    uscope::parse_value_expression("slice[0..2]").expect("parse slice range");
                let range_page = scenario
                    .operation(
                        "inspect one bounded Rust slice range",
                        scenario
                            .handle()
                            .inspect_range(range.expression, range.range.expect("terminal range")),
                    )
                    .await;
                assert_eq!(range_page.children.len(), 2, "{fixture}: {range_page:?}");
                let out_of_bounds = scenario
                    .handle()
                    .inspect(parsed_value_expression("slice[2]"))
                    .await;
                assert!(
                    matches!(
                        out_of_bounds,
                        Ok(uscope::InspectedValue {
                            state: VariableState::Unavailable(
                                uscope::VariableUnavailableReason::IndexOutOfBounds {
                                    index: 2,
                                    lower_bound: 0,
                                    count: 2,
                                }
                            ),
                            ..
                        })
                    ),
                    "{fixture}: {out_of_bounds:?}"
                );
            } else {
                assert!(
                    matches!(
                        slice.type_info.as_ref().map(|info| &info.kind),
                        Some(uscope::TypeKind::Slice { .. })
                    ) && matches!(
                        slice.state,
                        VariableState::Unavailable(uscope::VariableUnavailableReason::Unsupported(
                            uscope::UnsupportedVariableFeature::CompositeLocation
                        ))
                    ),
                    "{fixture}: {slice:?}"
                );
            }
        }
        if source == "variables.zig" {
            if fixture != "variables-zig-o2" {
                assert_dereferenced_scalar(
                    &dereference_named(&scenario, "pointer_parameter", 1).await,
                    42,
                    fixture,
                );
            }
            assert_dereferenced_scalar(
                &dereference_named(&scenario, "const_pointer", 1).await,
                42,
                fixture,
            );
            if fixture != "variables-zig-o2" {
                assert_dereferenced_scalar(
                    &dereference_named(&scenario, "alias_pointer", 1).await,
                    42,
                    fixture,
                );
            }
            assert_dereferenced_scalar(
                &dereference_named(&scenario, "many_pointer", 1).await,
                42,
                fixture,
            );
        }
        scenario.shutdown().await;
    }

    let fixture = "variables-zig-o2";
    let mut parameter = Scenario::new(
        "optimized Zig pointer parameter",
        Scenario::fixture(fixture),
    );
    parameter.add_source_breakpoint("variables.zig", 58).await;
    assert!(matches!(
        parameter.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    assert_dereferenced_scalar(
        &dereference_named(&parameter, "pointer_parameter", 1).await,
        42,
        fixture,
    );
    parameter.shutdown().await;
}

#[tokio::test]
async fn record_pointees_are_bounded_values_and_unsupported_pointees_remain_printable() {
    for (fixture, source, line, record_pointers, unsupported_pointers) in [
        (
            "variables-gcc-o0",
            "variables.c",
            68,
            &["structure_pointer", "recursive_pointer"][..],
            &["function_pointer"][..],
        ),
        (
            "variables-cpp-gcc-o0",
            "variables.cpp",
            50,
            &["structure_pointer", "recursive_pointer"][..],
            &[][..],
        ),
        (
            "variables-rust-o0",
            "variables.rs",
            67,
            &["structure_pointer", "recursive_pointer"][..],
            &[][..],
        ),
        (
            "variables-zig-o0",
            "variables.zig",
            73,
            &["structure_pointer", "recursive_pointer"][..],
            &[][..],
        ),
    ] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_source_breakpoint(source, line).await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        if fixture != "variables-zig-o0" {
            let pair = scenario
                .operation("inspect direct record", scenario.handle().variable("pair"))
                .await;
            record_page(&scenario, &pair.state, 2, &format!("{fixture} pair")).await;
        }
        for name in record_pointers {
            let value = dereference_named(&scenario, name, 1).await;
            assert_dereferenced_record(&scenario, &value, 2, &format!("{fixture} {name}")).await;
        }
        for name in unsupported_pointers {
            let variable = scenario
                .operation(
                    "inspect unsupported pointee",
                    scenario.handle().variable(*name),
                )
                .await;
            assert!(
                matches!(
                    variable.state,
                    VariableState::Available {
                        dereference: uscope::DereferenceState::Unavailable {
                            reason: uscope::DereferenceUnavailableReason::UnsupportedPointee(_),
                            ..
                        },
                        ..
                    }
                ) && matches!(
                    available_value(&variable.state),
                    uscope::VariableValue::Address(_)
                ),
                "{fixture} {name}: {variable:?}"
            );
        }
        let array = scenario
            .operation(
                "dereference bounded array",
                scenario.handle().variable("array_pointer"),
            )
            .await;
        let uscope::VariableState::Available {
            dereference: uscope::DereferenceState::Available(reference),
            ..
        } = array.state
        else {
            panic!("{fixture} array pointer did not expose a dereference: {array:?}");
        };
        let value = scenario
            .operation(
                "read bounded array",
                scenario.handle().dereference(reference),
            )
            .await;
        assert_array_values(&scenario, &value, fixture).await;
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn c_records_cover_nesting_arrays_bit_fields_globals_and_optimization() {
    for (fixture, inspect_parameters) in [
        ("records-c-gcc-o0", true),
        ("records-c-clang-o0", true),
        ("records-c-gcc-o2", false),
        ("records-c-clang-o2", false),
    ] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_breakpoint("inspect_records").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let global = scenario
            .operation(
                "inspect global record",
                scenario.handle().variable("global_record"),
            )
            .await;
        let global_page = record_page(&scenario, &global.state, 2, fixture).await;
        assert!(
            global_page
                .children
                .iter()
                .filter_map(|child| match &child.relationship {
                    uscope::ValueChildRelationship::Member(member) => Some(member),
                    _ => None,
                })
                .all(|member| member.declaration.is_some()),
            "{fixture}: record member declarations were not preserved: {global:?}"
        );
        let inner = named_child(&global_page, "inner");
        let inner_page = record_page(&scenario, &inner.state, 2, fixture).await;
        assert_signed_state(&named_child(&inner_page, "signed_value").state, -7);

        if inspect_parameters {
            let record = dereference_named(&scenario, "record", 1).await;
            assert_dereferenced_record(&scenario, &record, 2, fixture).await;

            let bits = dereference_named(&scenario, "bits", 1).await;
            let bits_page = record_page(&scenario, &bits.state, 3, fixture).await;
            assert_signed_state(&named_child(&bits_page, "negative").state, -3);
            assert!(matches!(
                available_value(&named_child(&bits_page, "first").state),
                uscope::VariableValue::Scalar(ScalarValue::Unsigned(5))
            ));
            assert!(matches!(
                available_value(&named_child(&bits_page, "second").state),
                uscope::VariableValue::Scalar(ScalarValue::Unsigned(42))
            ));

            let records = dereference_named(&scenario, "records", 1).await;
            let uscope::VariableValue::Array { .. } = available_value(&records.state) else {
                panic!("{fixture}: pointer-to-array did not decode: {records:?}");
            };
            let records_page = child_page(&scenario, &records.state, 0, 2).await;
            assert_eq!(records_page.children.len(), 2, "{fixture}: {records:?}");
            let second_page =
                record_page(&scenario, &records_page.children[1].state, 2, fixture).await;
            let values = named_child(&second_page, "values");
            let uscope::VariableValue::Array { .. } = available_value(&values.state) else {
                panic!("{fixture}: nested array was not decoded: {records:?}");
            };
            let values_page = child_page(&scenario, &values.state, 0, 2).await;
            assert_signed_state(&values_page.children[1].state, 44);

            let flexible = dereference_named(&scenario, "flexible", 1).await;
            let flexible_page = record_page(&scenario, &flexible.state, 2, fixture).await;
            assert_signed_state(&named_child(&flexible_page, "count").state, 2);
            assert!(matches!(
                named_child(&flexible_page, "values").state,
                VariableState::Unavailable(_)
            ));

            let incomplete = scenario
                .operation(
                    "inspect incomplete record pointer",
                    scenario.handle().variable("incomplete"),
                )
                .await;
            assert!(matches!(
                incomplete.state,
                VariableState::Available {
                    dereference: uscope::DereferenceState::Unavailable {
                        reason: uscope::DereferenceUnavailableReason::UnsupportedPointee(_),
                        ..
                    },
                    ..
                }
            ));
        }

        let mut reason = scenario.resume_to_stop().await;
        for _ in 0..8 {
            if matches!(reason, StopReason::Breakpoint { .. }) {
                reason = scenario.resume_to_stop().await;
            } else {
                break;
            }
        }
        assert_eq!(reason, StopReason::Exited(ExitStatus::Code(0)));
        scenario.shutdown().await;
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario proves lazy record and array access plus the existing structural path across both C compilers"
)]
async fn structural_inspection_reads_a_small_field_without_materializing_a_large_record() {
    for fixture in ["records-c-gcc-o0", "records-c-clang-o0"] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_breakpoint("inspect_records").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let whole_record = dereference_named(&scenario, "large", 1).await;
        assert!(
            matches!(
                whole_record.state,
                VariableState::Available {
                    raw: None,
                    value: uscope::VariableValue::Record,
                    ..
                }
            ) && available_children(&whole_record.state).total() == 2,
            "{fixture}: the large record was eagerly read: {whole_record:?}"
        );
        let tail = child_page(&scenario, &whole_record.state, 1, 1).await;
        assert_eq!(tail.children.len(), 1, "{fixture}: {tail:?}");
        assert_signed_state(&tail.children[0].state, 73);
        let header = child_page(&scenario, &whole_record.state, 0, 1).await;
        let padding = &header.children[0];
        assert!(
            matches!(
                available_value(&padding.state),
                uscope::VariableValue::Array { .. }
            ) && available_children(&padding.state).total() == 2048,
            "{fixture}: large member did not remain lazy: {padding:?}"
        );
        let middle = child_page(&scenario, &padding.state, 1024, 256).await;
        assert_eq!(middle.children.len(), 256, "{fixture}: {middle:?}");
        assert!(middle.children.iter().all(|child| matches!(
            available_value(&child.state),
            uscope::VariableValue::Scalar(ScalarValue::Unsigned(0))
        )));
        assert!(matches!(
            &middle.children[255].relationship,
            uscope::ValueChildRelationship::ArrayElement {
                index: 1279,
                indices,
            } if indices.as_ref() == [1279]
        ));
        let huge = scenario
            .operation(
                "inspect array above the former eager limit",
                scenario.handle().variable("huge_array"),
            )
            .await;
        assert!(
            matches!(
                huge.state,
                VariableState::Available {
                    raw: None,
                    value: uscope::VariableValue::Array { .. },
                    ..
                }
            ) && available_children(&huge.state).total() == 1024 * 1024 + 1,
            "{fixture}: large array was rejected or read eagerly: {huge:?}"
        );
        let huge_tail = child_page(&scenario, &huge.state, 1024 * 1024, 1).await;
        assert!(matches!(
            available_value(&huge_tail.children[0].state),
            uscope::VariableValue::Scalar(ScalarValue::Unsigned(0))
        ));
        for expression in ["huge_array[0]", "huge_array[1048576]"] {
            let indexed = scenario
                .operation(
                    "inspect one large-array element directly",
                    scenario
                        .handle()
                        .inspect(parsed_value_expression(expression)),
                )
                .await;
            assert!(
                matches!(
                    available_value(&indexed.state),
                    uscope::VariableValue::Scalar(ScalarValue::Unsigned(0))
                ),
                "{fixture}: {expression}: {indexed:?}"
            );
        }
        let nested_array_member = scenario
            .operation(
                "inspect through an explicitly dereferenced array",
                scenario
                    .handle()
                    .inspect(parsed_value_expression("(*records)[1].values[1]")),
            )
            .await;
        assert_inspected_signed(&nested_array_member, 44, fixture);
        let matrix_element = scenario
            .operation(
                "inspect one multidimensional array element",
                scenario
                    .handle()
                    .inspect(parsed_value_expression("matrix[1][2]")),
            )
            .await;
        assert_inspected_signed(&matrix_element, 6, fixture);
        let range =
            uscope::parse_value_expression("huge_array[3..7]").expect("parse bounded array range");
        let range_page = scenario
            .operation(
                "inspect one bounded array range",
                scenario.handle().inspect_range(
                    range.expression,
                    range.range.expect("parsed terminal range"),
                ),
            )
            .await;
        assert_eq!(range_page.offset, 3, "{fixture}: {range_page:?}");
        assert_eq!(range_page.children.len(), 4, "{fixture}: {range_page:?}");
        assert!(range_page.children.iter().enumerate().all(|(relative, child)| {
            matches!(
                &child.relationship,
                uscope::ValueChildRelationship::ArrayElement { index, indices }
                    if *index == 3 + u64::try_from(relative).expect("small index")
                        && indices.as_ref() == [i128::try_from(3 + relative).expect("small index")]
            )
        }));
        let empty = uscope::parse_value_expression("huge_array[7..7]").expect("parse empty range");
        let empty_page = scenario
            .operation(
                "inspect one empty in-bounds range",
                scenario
                    .handle()
                    .inspect_range(empty.expression, empty.range.expect("terminal range")),
            )
            .await;
        assert_eq!(empty_page.offset, 7, "{fixture}: {empty_page:?}");
        assert!(empty_page.children.is_empty(), "{fixture}: {empty_page:?}");
        for expression in [
            "huge_array[7..3]",
            "huge_array[0..257]",
            "huge_array[1048576..1048578]",
            "matrix[0..1]",
            "global_record[0..1]",
        ] {
            let parsed =
                uscope::parse_value_expression(expression).expect("parse invalid semantic range");
            let result = scenario
                .handle()
                .inspect_range(parsed.expression, parsed.range.expect("terminal range"))
                .await;
            assert!(result.is_err(), "{fixture}: {expression}: {result:?}");
        }
        let out_of_bounds = scenario
            .handle()
            .inspect(parsed_value_expression("huge_array[1048577]"))
            .await;
        assert!(
            matches!(
                out_of_bounds,
                Err(uscope::Error::ValueIndexOutOfBounds { .. })
            ),
            "{fixture}: {out_of_bounds:?}"
        );
        let pointer_index = scenario
            .handle()
            .inspect(parsed_value_expression("records[0]"))
            .await;
        assert!(
            matches!(
                pointer_index,
                Err(uscope::Error::IndexAccessOnNonIndexable { .. })
            ),
            "{fixture}: {pointer_index:?}"
        );
        for (components, expected) in [
            (&["record", "inner", "signed_value"][..], -7),
            (&["bits", "negative"][..], -3),
        ] {
            let value = scenario
                .operation(
                    "inspect nested or bit-field member",
                    scenario.handle().inspect(value_expression(components)),
                )
                .await;
            assert_inspected_signed(&value, expected, fixture);
        }
        let unsigned_bit_field = scenario
            .operation(
                "inspect unsigned bit-field member",
                scenario
                    .handle()
                    .inspect(value_expression(&["bits", "second"])),
            )
            .await;
        assert!(
            matches!(
                available_value(&unsigned_bit_field.state),
                uscope::VariableValue::Scalar(ScalarValue::Unsigned(42))
            ),
            "{fixture}: {unsigned_bit_field:?}"
        );

        let selected = scenario
            .operation(
                "inspect small field in large record",
                scenario
                    .handle()
                    .inspect(value_expression(&["large", "small"])),
            )
            .await;
        assert_inspected_signed(&selected, 73, fixture);

        scenario.shutdown().await;
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one public scenario proves every budget resource and resumable partial pages"
)]
async fn inspection_budgets_report_typed_partial_results_at_each_public_boundary() {
    for fixture in ["records-c-gcc-o0", "records-c-clang-o0"] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_breakpoint("inspect_records").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let limits = uscope::InspectionLimits {
            variables: 1,
            ..uscope::InspectionLimits::default()
        };
        let variables = scenario
            .operation(
                "truncate visible variables at the exact variable boundary",
                scenario.handle().variables_with_limits(limits),
            )
            .await;
        assert_eq!(variables.variables.len(), 1, "{variables:?}");
        assert!(matches!(
            variables.completion,
            uscope::InspectionCompletion::Truncated(uscope::InspectionExhaustion {
                resource: uscope::InspectionLimit::Variables,
                limit: 1,
                used: 1,
                requested: 1,
            })
        ));

        let huge = scenario
            .operation(
                "obtain a stop-scoped large-array capability",
                scenario
                    .handle()
                    .inspect(parsed_value_expression("huge_array")),
            )
            .await;
        let reference = available_children(&huge.state).clone();

        let bounded_range =
            uscope::parse_value_expression("huge_array[0..4]").expect("parse bounded range");
        let range = scenario
            .operation(
                "share one value-node budget across range selection and its child page",
                scenario.handle().inspect_range_with_limits(
                    bounded_range.expression,
                    bounded_range.range.expect("terminal range"),
                    uscope::InspectionLimits {
                        value_nodes: 3,
                        ..uscope::InspectionLimits::default()
                    },
                ),
            )
            .await;
        assert_eq!(range.children.len(), 2, "{range:?}");
        assert!(matches!(
            range.completion,
            uscope::InspectionCompletion::Truncated(uscope::InspectionExhaustion {
                resource: uscope::InspectionLimit::ValueNodes,
                limit: 3,
                used: 3,
                requested: 1,
            })
        ));

        let node_limits = uscope::InspectionLimits {
            value_nodes: 2,
            ..uscope::InspectionLimits::default()
        };
        let nodes = scenario
            .operation(
                "truncate a child page by value nodes",
                scenario.handle().value_children_with_limits(
                    reference.clone(),
                    uscope::ValueChildQuery {
                        offset: 0,
                        limit: 4,
                    },
                    node_limits,
                ),
            )
            .await;
        assert_eq!(nodes.children.len(), 2, "{nodes:?}");
        assert!(matches!(
            nodes.completion,
            uscope::InspectionCompletion::Truncated(uscope::InspectionExhaustion {
                resource: uscope::InspectionLimit::ValueNodes,
                limit: 2,
                used: 2,
                requested: 1,
            })
        ));
        let resumed = scenario
            .operation(
                "resume after a truncated child prefix with a fresh budget",
                scenario.handle().value_children_with_limits(
                    reference.clone(),
                    uscope::ValueChildQuery {
                        offset: 2,
                        limit: 2,
                    },
                    node_limits,
                ),
            )
            .await;
        assert!(matches!(
            resumed.children.as_ref(),
            [first, second]
                if matches!(
                    &first.relationship,
                    uscope::ValueChildRelationship::ArrayElement { index: 2, .. }
                ) && matches!(
                    &second.relationship,
                    uscope::ValueChildRelationship::ArrayElement { index: 3, .. }
                )
        ));

        let byte_limits = uscope::InspectionLimits {
            memory_bytes: 2,
            ..uscope::InspectionLimits::default()
        };
        let bytes = scenario
            .operation(
                "truncate a child page by requested memory bytes",
                scenario.handle().value_children_with_limits(
                    reference.clone(),
                    uscope::ValueChildQuery {
                        offset: 0,
                        limit: 4,
                    },
                    byte_limits,
                ),
            )
            .await;
        assert_eq!(bytes.children.len(), 2, "{bytes:?}");
        assert_eq!(bytes.usage.memory_bytes, 2, "{bytes:?}");
        assert!(matches!(
            bytes.completion,
            uscope::InspectionCompletion::Truncated(uscope::InspectionExhaustion {
                resource: uscope::InspectionLimit::MemoryBytes,
                limit: 2,
                used: 2,
                requested: 1,
            })
        ));

        let read_limits = uscope::InspectionLimits {
            memory_reads: 1,
            memory_bytes: 3,
            ..uscope::InspectionLimits::default()
        };
        let reads = scenario
            .operation(
                "truncate a child page by logical memory reads",
                scenario.handle().value_children_with_limits(
                    reference,
                    uscope::ValueChildQuery {
                        offset: 0,
                        limit: 4,
                    },
                    read_limits,
                ),
            )
            .await;
        assert_eq!(reads.children.len(), 1, "{reads:?}");
        assert_eq!(reads.usage.memory_reads, 1, "{reads:?}");
        assert!(matches!(
            reads.completion,
            uscope::InspectionCompletion::Truncated(uscope::InspectionExhaustion {
                resource: uscope::InspectionLimit::MemoryReads,
                limit: 1,
                used: 1,
                requested: 1,
            })
        ));

        let depth_limits = uscope::InspectionLimits {
            aggregate_depth: 1,
            ..uscope::InspectionLimits::default()
        };
        let depth = scenario
            .operation(
                "truncate a structural path by aggregate depth",
                scenario.handle().inspect_with_limits(
                    parsed_value_expression("global_record.inner.signed_value"),
                    depth_limits,
                ),
            )
            .await;
        assert!(matches!(
            depth.completion,
            uscope::InspectionCompletion::Truncated(uscope::InspectionExhaustion {
                resource: uscope::InspectionLimit::AggregateDepth,
                limit: 1,
                used: 0,
                requested: 2,
            })
        ));

        let work_limits = uscope::InspectionLimits {
            expression_work: 1,
            ..uscope::InspectionLimits::default()
        };
        let work = scenario
            .operation(
                "truncate DWARF evaluation by expression work",
                scenario
                    .handle()
                    .inspect_with_limits(parsed_value_expression("huge_array[0]"), work_limits),
            )
            .await;
        assert!(matches!(
            work.completion,
            uscope::InspectionCompletion::Truncated(uscope::InspectionExhaustion {
                resource: uscope::InspectionLimit::ExpressionWork,
                limit: 1,
                used: 1,
                requested: 10_000,
            })
        ));

        let invalid = uscope::InspectionLimits {
            memory_reads: 0,
            ..uscope::InspectionLimits::default()
        };
        assert!(matches!(
            scenario
                .handle()
                .inspect_with_limits(parsed_value_expression("huge_array"), invalid)
                .await,
            Err(uscope::Error::InvalidInspectionLimit {
                resource: uscope::InspectionLimit::MemoryReads,
                value: 0,
                maximum: 1_024,
            })
        ));

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn structural_inspection_preserves_dots_in_global_roots_before_selecting_members() {
    let fixture = "records-go-o0";
    let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
    scenario.add_source_breakpoint("main.go", 22).await;
    run_go_to_breakpoint(&mut scenario, fixture).await;

    let root = scenario
        .operation(
            "inspect dotted global root",
            scenario
                .handle()
                .inspect(value_expression(&["main", "globalRecord"])),
        )
        .await;
    record_page(&scenario, &root.state, 2, fixture).await;

    let member = scenario
        .operation(
            "inspect member below dotted global root",
            scenario.handle().inspect(value_expression(&[
                "main",
                "globalRecord",
                "inner",
                "signedValue",
            ])),
        )
        .await;
    assert_inspected_signed(&member, -7, fixture);

    resume_go_to_exit(&mut scenario, fixture).await;
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

#[tokio::test]
async fn cpp_records_cover_multiple_and_virtual_base_metadata() {
    for fixture in [
        "records-cpp-gcc-o0",
        "records-cpp-clang-o0",
        "records-cpp-gcc-o2",
        "records-cpp-clang-o2",
    ] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_breakpoint("inspect_records").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let derived = dereference_named(&scenario, "derived", 1).await;
        let derived_page = record_page(&scenario, &derived.state, 3, fixture).await;
        let bases = derived_page
            .children
            .iter()
            .filter(|child| matches!(child.relationship, uscope::ValueChildRelationship::Base(_)))
            .collect::<Vec<_>>();
        assert_eq!(bases.len(), 2, "{fixture}: {derived:?}");
        assert!(
            derived_page
                .children
                .iter()
                .filter_map(|child| match &child.relationship {
                    uscope::ValueChildRelationship::Member(member) => Some(member),
                    _ => None,
                })
                .all(|member| member.name.as_deref() != Some("static_value")),
            "{fixture}: static member appeared in instance: {derived:?}"
        );
        assert_signed_state(&named_child(&derived_page, "own").state, 22);
        let left_page = record_page(&scenario, &bases[0].state, 1, fixture).await;
        assert_signed_state(&named_child(&left_page, "left").state, 9);
        let right_page = record_page(&scenario, &bases[1].state, 1, fixture).await;
        assert_signed_state(&named_child(&right_page, "right").state, 11);

        let virtual_derived = dereference_named(&scenario, "virtual_derived", 1).await;
        let virtual_page = record_page(&scenario, &virtual_derived.state, 1, fixture).await;
        let bases = virtual_page
            .children
            .iter()
            .filter(|child| matches!(child.relationship, uscope::ValueChildRelationship::Base(_)))
            .collect::<Vec<_>>();
        assert_eq!(bases.len(), 1, "{fixture}: {virtual_derived:?}");
        assert!(matches!(
            bases[0].relationship,
            uscope::ValueChildRelationship::Base(uscope::BaseClass {
                virtuality: uscope::BaseClassVirtuality::Virtual,
                ..
            })
        ));
        let virtual_base_page = record_page(&scenario, &bases[0].state, 1, fixture).await;
        assert_signed_state(&named_child(&virtual_base_page, "virtual_value").state, 22);

        let diamond = dereference_named(&scenario, "diamond", 1).await;
        let diamond_page = record_page(&scenario, &diamond.state, 2, fixture).await;
        let bases = diamond_page
            .children
            .iter()
            .filter(|child| matches!(child.relationship, uscope::ValueChildRelationship::Base(_)))
            .collect::<Vec<_>>();
        assert_eq!(bases.len(), 2, "{fixture}: {diamond:?}");
        for branch in &bases {
            let branch_page = record_page(&scenario, &branch.state, 1, fixture).await;
            let root = branch_page
                .children
                .iter()
                .find(|child| matches!(child.relationship, uscope::ValueChildRelationship::Base(_)))
                .unwrap_or_else(|| {
                    panic!("{fixture}: diamond branch had no base: {branch_page:?}")
                });
            assert!(
                matches!(available_value(&root.state), uscope::VariableValue::Record),
                "{fixture}: virtual root was not lazy-expandable: {root:?}"
            );
        }

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one matrix verifies equivalent record, array, slice, and optimized behavior across three producers"
)]
async fn rust_zig_and_go_records_cover_nested_arrays_slices_and_optimized_metadata() {
    for (fixture, function, inspect_values) in [
        ("records-rust-o0", "inspect_records", true),
        ("records-rust-o2", "inspect_records", false),
        ("records-zig-o0", "records.inspectRecords", true),
        ("records-zig-o2", "records.inspectRecords", false),
        ("records-zig-nopie", "records.inspectRecords", true),
    ] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        if fixture.contains("zig") {
            scenario.add_source_breakpoint("records.zig", 28).await;
        } else {
            scenario.add_breakpoint(function).await;
        }
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        for name in ["record", "records"] {
            if !inspect_values {
                let variable = scenario
                    .operation(
                        "inspect optimized record metadata",
                        scenario.handle().variable(name),
                    )
                    .await;
                assert!(
                    variable.type_info.is_some(),
                    "{fixture} {name}: {variable:?}"
                );
                assert!(!matches!(variable.state, VariableState::Malformed(_)));
                continue;
            }
            let value = dereference_named(&scenario, name, 1).await;
            match available_value(&value.state) {
                uscope::VariableValue::Record => {
                    record_page(&scenario, &value.state, 2, &format!("{fixture} {name}")).await;
                }
                uscope::VariableValue::Array { .. } => {
                    let page = child_page(&scenario, &value.state, 0, 2).await;
                    assert_eq!(page.children.len(), 2, "{fixture}: {value:?}");
                    assert!(matches!(
                        available_value(&page.children[0].state),
                        uscope::VariableValue::Record
                    ));
                }
                other => panic!("{fixture} {name}: unexpected value {other:?}"),
            }
        }
        if inspect_values {
            let slice = scenario
                .operation("inspect record slice", scenario.handle().variable("slice"))
                .await;
            let uscope::VariableValue::Slice { length: 2, .. } = available_value(&slice.state)
            else {
                panic!("{fixture}: slice did not decode: {slice:?}");
            };
            let slice_page = child_page(&scenario, &slice.state, 0, 2).await;
            assert_eq!(slice_page.children.len(), 2, "{fixture}: {slice:?}");
            assert!(matches!(
                available_value(&slice_page.children[0].state),
                uscope::VariableValue::Record
            ));
            if fixture.starts_with("records-zig-") {
                let packed = dereference_named(&scenario, "packed_record", 1).await;
                let packed_page = record_page(&scenario, &packed.state, 1, fixture).await;
                assert_eq!(packed_page.children.len(), 1, "{packed:?}");
                assert_eq!(
                    match &packed_page.children[0].relationship {
                        uscope::ValueChildRelationship::Member(member) => member.name.as_deref(),
                        _ => None,
                    },
                    Some("bits"),
                    "{packed:?}"
                );
            }
        }
        let mut reason = scenario.resume_to_stop().await;
        for _ in 0..8 {
            if matches!(reason, StopReason::Breakpoint { .. }) {
                reason = scenario.resume_to_stop().await;
            } else {
                break;
            }
        }
        assert_eq!(reason, StopReason::Exited(ExitStatus::Code(0)));
        scenario.shutdown().await;
    }

    for (fixture, inspect_values) in [("records-go-o0", true), ("records-go-o2", false)] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_source_breakpoint("main.go", 22).await;
        run_go_to_breakpoint(&mut scenario, fixture).await;
        if inspect_values {
            let record = dereference_named(&scenario, "record", 1).await;
            assert_dereferenced_record(&scenario, &record, 2, fixture).await;
            let record_page = record_page(&scenario, &record.state, 2, fixture).await;
            assert!(
                record_page.children.iter().any(|child| matches!(
                    &child.relationship,
                    uscope::ValueChildRelationship::Member(member) if member.embedded
                )),
                "{fixture}: Go embedded-field metadata was lost: {record:?}"
            );
            let records = dereference_named(&scenario, "records", 1).await;
            assert!(matches!(
                available_value(&records.state),
                uscope::VariableValue::Array { .. }
            ));
            assert_eq!(available_children(&records.state).total(), 2);
            let slice = scenario
                .operation(
                    "inspect Go record slice",
                    scenario.handle().variable("slice"),
                )
                .await;
            assert!(matches!(
                available_value(&slice.state),
                uscope::VariableValue::Slice { length: 2, .. }
            ));
        } else {
            let variable = scenario
                .operation(
                    "inspect optimized Go global record",
                    scenario.handle().variable("main.globalRecord"),
                )
                .await;
            record_page(&scenario, &variable.state, 2, fixture).await;
        }
        resume_go_to_exit(&mut scenario, fixture).await;
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

#[tokio::test]
async fn rust_payload_enum_is_never_published_as_an_empty_record() {
    let fixture = "enums-rust-o0";
    let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
    scenario.add_breakpoint("inspect_enum").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    let value = dereference_named(&scenario, "value", 1).await;
    let uscope::VariableValue::Variant {
        discriminant,
        active: Some(active),
    } = available_value(&value.state)
    else {
        panic!("payload enum did not decode as an active variant: {value:?}");
    };
    assert_eq!(
        *discriminant,
        Some(uscope::IntegerValue::Unsigned(1)),
        "{value:?}"
    );
    assert_eq!(active.members.len(), 1, "{value:?}");
    assert_eq!(
        active.members[0].name.as_deref(),
        Some("Integer"),
        "{value:?}"
    );
    let variant_page = record_page(&scenario, &value.state, 1, fixture).await;
    let integer = named_child(&variant_page, "Integer");
    let payload_page = record_page(&scenario, &integer.state, 1, fixture).await;
    let payload = named_child(&payload_page, "__0");
    assert!(
        matches!(
            available_value(&payload.state),
            uscope::VariableValue::Scalar(ScalarValue::Unsigned(42))
        ),
        "{value:?}"
    );

    let selected_payload = scenario
        .operation(
            "select active Rust enum payload",
            scenario
                .handle()
                .inspect(value_expression(&["value", "Integer", "__0"])),
        )
        .await;
    assert!(
        matches!(
            available_value(&selected_payload.state),
            uscope::VariableValue::Scalar(ScalarValue::Unsigned(42))
        ),
        "{selected_payload:?}"
    );
    let inactive_payload = scenario
        .operation(
            "reject inactive Rust enum payload",
            scenario
                .handle()
                .inspect(value_expression(&["value", "Unit"])),
        )
        .await;
    assert!(
        matches!(
            inactive_payload.state,
            VariableState::Unavailable(uscope::VariableUnavailableReason::Other(ref reason))
                if reason.contains("inactive arm")
        ),
        "{inactive_payload:?}"
    );

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one matrix keeps identical symbolic-value assertions aligned across C and Rust producers"
)]
async fn c_and_rust_fieldless_enums_preserve_values_names_aliases_and_unknowns() {
    for fixture in [
        "enums-c-gcc-o0",
        "enums-c-clang-o0",
        "enums-c-gcc-o2",
        "enums-c-clang-o2",
    ] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_breakpoint("inspect_enums").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        for (name, expected, expected_names) in [
            (
                "signed_value",
                uscope::IntegerValue::Signed(-3),
                &["SIGNED_NEGATIVE"][..],
            ),
            (
                "zero_alias",
                uscope::IntegerValue::Signed(0),
                &["SIGNED_ZERO", "SIGNED_ZERO_ALIAS"][..],
            ),
            ("flags", uscope::IntegerValue::Unsigned(3), &[][..]),
            (
                "byte_value",
                uscope::IntegerValue::Unsigned(255),
                &["BYTE_MAX"][..],
            ),
        ] {
            let inspected = dereference_named(&scenario, name, 1).await;
            let uscope::VariableValue::Enumeration { value, matches } =
                available_value(&inspected.state)
            else {
                panic!("{fixture} {name}: value was not an enumeration: {inspected:?}");
            };
            assert_eq!(*value, expected, "{fixture} {name}: {inspected:?}");
            assert_eq!(
                matches
                    .iter()
                    .map(|enumerator| enumerator.name.as_ref())
                    .collect::<Vec<_>>(),
                expected_names,
                "{fixture} {name}: {inspected:?}"
            );
        }

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        scenario.shutdown().await;
    }

    for fixture in ["enums-rust-o0", "enums-rust-o2"] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_breakpoint("inspect_enum").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let fieldless = dereference_named(&scenario, "fieldless", 1).await;
        assert!(
            matches!(
                available_value(&fieldless.state),
                uscope::VariableValue::Enumeration {
                    value: uscope::IntegerValue::Signed(-3),
                    matches,
                } if matches.len() == 1 && matches[0].name.as_ref() == "Negative"
            ),
            "{fieldless:?}"
        );
        let payload = dereference_named(&scenario, "value", 1).await;
        assert!(
            matches!(
                available_value(&payload.state),
                uscope::VariableValue::Variant {
                    active: Some(active),
                    ..
                } if active.members.first().and_then(|member| member.name.as_deref())
                    == Some("Integer")
            ),
            "{fixture}: {payload:?}"
        );
        let wide = dereference_named(&scenario, "wide", 1).await;
        assert!(
            matches!(
                available_value(&wide.state),
                uscope::VariableValue::Enumeration {
                    value: uscope::IntegerValue::Unsigned(value),
                    matches,
                } if *value == (1_u128 << 100) + 9
                    && matches.len() == 1
                    && matches[0].name.as_ref() == "Huge"
            ),
            "{fixture}: {wide:?}"
        );
        let optional = dereference_named(&scenario, "optional", 1).await;
        assert!(
            matches!(
                available_value(&optional.state),
                uscope::VariableValue::Variant {
                    active: Some(active),
                    ..
                } if active.members.first().and_then(|member| member.name.as_deref())
                    == Some("Some")
            ),
            "{fixture}: {optional:?}"
        );
        let empty = dereference_named(&scenario, "empty", 1).await;
        assert!(
            matches!(
                available_value(&empty.state),
                uscope::VariableValue::Variant {
                    active: Some(active),
                    ..
                } if active.members.first().and_then(|member| member.name.as_deref())
                    == Some("None")
            ),
            "{fixture}: {empty:?}"
        );
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn go_named_integer_constants_reconstruct_symbolic_values() {
    for fixture in ["enums-go-o0", "enums-go-o2"] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_breakpoint("main.inspectEnums").await;
        run_go_to_breakpoint(&mut scenario, fixture).await;

        for (name, expected, expected_names) in [
            (
                "negative",
                uscope::IntegerValue::Signed(-3),
                &["main.StateNegative"][..],
            ),
            (
                "alias",
                uscope::IntegerValue::Signed(0),
                &["main.StateZero", "main.StateAlias"][..],
            ),
            ("unknown", uscope::IntegerValue::Signed(5), &[][..]),
        ] {
            let inspected = dereference_named(&scenario, name, 1).await;
            let uscope::VariableValue::Enumeration { value, matches } =
                available_value(&inspected.state)
            else {
                panic!("{fixture} {name}: value was not symbolic: {inspected:?}");
            };
            assert_eq!(*value, expected, "{fixture} {name}: {inspected:?}");
            assert_eq!(
                matches
                    .iter()
                    .map(|enumerator| enumerator.name.as_ref())
                    .collect::<Vec<_>>(),
                expected_names,
                "{fixture} {name}: {inspected:?}"
            );
        }

        resume_go_to_exit(&mut scenario, fixture).await;
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

#[tokio::test]
async fn c_raw_unions_expose_overlapping_interpretations_without_claiming_an_active_member() {
    for fixture in [
        "enums-c-gcc-o0",
        "enums-c-clang-o0",
        "enums-c-gcc-o2",
        "enums-c-clang-o2",
    ] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_breakpoint("inspect_enums").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let raw = dereference_named(&scenario, "raw", 1).await;
        let uscope::VariableValue::Union = available_value(&raw.state) else {
            panic!("{fixture}: raw value was not a union: {raw:?}");
        };
        let page = record_page(&scenario, &raw.state, 2, fixture).await;
        assert_eq!(
            page.children
                .iter()
                .filter_map(|child| match &child.relationship {
                    uscope::ValueChildRelationship::Member(member) => member.name.as_deref(),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            ["integer", "floating"],
            "{fixture}: {raw:?}"
        );
        assert_signed_state(&named_child(&page, "integer").state, 42);

        let integer = scenario
            .operation(
                "inspect a union interpretation",
                scenario
                    .handle()
                    .inspect(value_expression(&["raw", "integer"])),
            )
            .await;
        assert_inspected_signed(&integer, 42, fixture);

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn cpp_scoped_enums_and_unions_preserve_language_semantics() {
    for fixture in [
        "enums-cpp-gcc-o0",
        "enums-cpp-clang-o0",
        "enums-cpp-gcc-o2",
        "enums-cpp-clang-o2",
    ] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_breakpoint("inspect_enums").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        for (name, expected, expected_names) in [
            ("state", uscope::IntegerValue::Signed(-3), &["Negative"][..]),
            (
                "alias",
                uscope::IntegerValue::Signed(0),
                &["Zero", "Alias"][..],
            ),
        ] {
            let inspected = dereference_named(&scenario, name, 1).await;
            let uscope::VariableValue::Enumeration { value, matches } =
                available_value(&inspected.state)
            else {
                panic!("{fixture} {name}: value was not an enumeration: {inspected:?}");
            };
            assert_eq!(*value, expected, "{fixture} {name}: {inspected:?}");
            assert_eq!(
                matches
                    .iter()
                    .map(|enumerator| enumerator.name.as_ref())
                    .collect::<Vec<_>>(),
                expected_names,
                "{fixture} {name}: {inspected:?}"
            );
        }
        let raw = dereference_named(&scenario, "raw", 1).await;
        assert!(
            matches!(available_value(&raw.state), uscope::VariableValue::Union)
                && available_children(&raw.state).total() == 2,
            "{fixture}: {raw:?}"
        );

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn zig_enums_tagged_unions_and_bare_unions_decode_without_guessing() {
    for fixture in ["enums-zig-o0", "enums-zig-o2"] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario
            .add_source_breakpoint("tests/fixtures/zig/enums.zig", 27)
            .await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let state = dereference_named(&scenario, "state", 1).await;
        let VariableState::Available {
            value: state_graph, ..
        } = &state.state
        else {
            panic!("{fixture}: state was unavailable: {state:?}");
        };
        assert!(
            matches!(
                state_graph,
                uscope::VariableValue::Enumeration {
                    value: uscope::IntegerValue::Signed(-3),
                    matches,
                } if matches.len() == 1 && matches[0].name.as_ref() == "negative"
            ),
            "{fixture}: {state:?}"
        );
        let tagged = dereference_named(&scenario, "tagged", 1).await;
        let VariableState::Available { value: graph, .. } = &tagged.state else {
            panic!("{fixture}: tagged union was unavailable: {tagged:?}");
        };
        let uscope::VariableValue::Variant {
            active: Some(active),
            ..
        } = graph
        else {
            panic!("{fixture}: tagged union did not select an arm: {tagged:?}");
        };
        let integer = active
            .members
            .iter()
            .find(|member| member.name.as_deref() == Some("integer"))
            .unwrap_or_else(|| panic!("{fixture}: integer arm missing: {tagged:?}"));
        assert_eq!(integer.name.as_deref(), Some("integer"));
        let tagged_page = record_page(&scenario, &tagged.state, 1, fixture).await;
        assert!(
            matches!(
                available_value(&named_child(&tagged_page, "integer").state),
                uscope::VariableValue::Scalar(ScalarValue::Unsigned(42))
            ),
            "{fixture}: {tagged:?}"
        );
        let raw = dereference_named(&scenario, "raw", 1).await;
        assert!(
            matches!(available_value(&raw.state), uscope::VariableValue::Union)
                && available_children(&raw.state).total() == 2,
            "{fixture}: {raw:?}"
        );
        for (name, arm, expected) in [("optional", "some", 43), ("failure", "success", 44)] {
            let inspected = dereference_named(&scenario, name, 1).await;
            let uscope::VariableValue::Variant {
                active: Some(active),
                ..
            } = available_value(&inspected.state)
            else {
                panic!("{fixture}: {name} did not select an arm: {inspected:?}");
            };
            assert_eq!(active.name.as_deref(), Some(arm), "{inspected:?}");
            assert_eq!(active.members.len(), 1, "{inspected:?}");
            let page = record_page(&scenario, &inspected.state, 1, fixture).await;
            assert!(
                matches!(
                    available_value(&page.children[0].state),
                    uscope::VariableValue::Scalar(ScalarValue::Unsigned(value))
                        if *value == expected
                ),
                "{fixture}: {inspected:?}"
            );
        }

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn dereference_reads_are_all_or_unavailable_across_an_unmapped_boundary() {
    for fixture in ["pointer-memory-gcc-o0", "pointer-memory-clang-o0"] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_breakpoint("inspect_boundaries").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let valid = dereference_named(&scenario, "valid_pointer", 1).await;
        assert_dereferenced_scalar(&valid, 42, fixture);
        let boundary = dereference_named(&scenario, "boundary_pointer", 1).await;
        assert!(
            matches!(boundary.state, VariableState::Unavailable(_)),
            "{boundary:?}"
        );
        let valid_after_failure = dereference_named(&scenario, "valid_pointer", 1).await;
        assert_dereferenced_scalar(&valid_after_failure, 42, fixture);

        let array = dereference_named(&scenario, "boundary_array", 1).await;
        assert!(
            matches!(
                array.state,
                VariableState::Available {
                    raw: None,
                    value: uscope::VariableValue::Array { .. },
                    ..
                }
            ) && available_children(&array.state).total() == 4,
            "{fixture}: array summary performed an eager boundary read: {array:?}"
        );
        let readable = child_page(&scenario, &array.state, 0, 2).await;
        assert_signed_state(&readable.children[0].state, 41);
        assert_signed_state(&readable.children[1].state, 42);
        let unreadable = child_page(&scenario, &array.state, 2, 2).await;
        assert!(
            unreadable
                .children
                .iter()
                .all(|child| matches!(child.state, VariableState::Unavailable(_))),
            "{fixture}: an unmapped child was reported as readable: {unreadable:?}"
        );
        assert_eq!(
            child_page(&scenario, &array.state, 2, 2).await,
            unreadable,
            "{fixture}: repeated page evaluation changed at one stop"
        );
        let indexed_readable = scenario
            .operation(
                "inspect a readable element at a mapping boundary",
                scenario
                    .handle()
                    .inspect(parsed_value_expression("(*boundary_array)[1]")),
            )
            .await;
        assert_inspected_signed(&indexed_readable, 42, fixture);
        let indexed_unreadable = scenario
            .operation(
                "inspect an unreadable element at a mapping boundary",
                scenario
                    .handle()
                    .inspect(parsed_value_expression("(*boundary_array)[2]")),
            )
            .await;
        assert!(
            matches!(indexed_unreadable.state, VariableState::Unavailable(_)),
            "{fixture}: {indexed_unreadable:?}"
        );
        let parsed = uscope::parse_value_expression("(*boundary_array)[0..4]")
            .expect("parse boundary range");
        let range = scenario
            .operation(
                "inspect a range crossing an unmapped boundary",
                scenario
                    .handle()
                    .inspect_range(parsed.expression, parsed.range.expect("terminal range")),
            )
            .await;
        assert_signed_state(&range.children[0].state, 41);
        assert_signed_state(&range.children[1].state, 42);
        assert!(
            range.children[2..]
                .iter()
                .all(|child| matches!(child.state, VariableState::Unavailable(_)))
        );

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn value_child_pages_are_arbitrary_repeatable_bounded_and_stop_scoped() {
    for fixture in ["variables-gcc-o0", "variables-clang-o0"] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_source_breakpoint("variables.c", 68).await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let array = dereference_named(&scenario, "array_pointer", 1).await;
        let reference = available_children(&array.state).clone();
        assert_eq!(reference.total(), 2);

        let tail = child_page(&scenario, &array.state, 1, 1).await;
        assert_eq!(tail.offset, 1);
        assert_eq!(tail.total, 2);
        assert_eq!(tail.children.len(), 1);
        assert_signed_state(&tail.children[0].state, 22);
        assert!(matches!(
            &tail.children[0].relationship,
            uscope::ValueChildRelationship::ArrayElement { index: 1, indices }
                if indices.as_ref() == [1]
        ));
        assert_eq!(
            child_page(&scenario, &array.state, 1, 1).await,
            tail,
            "{fixture}: a repeated page changed at the same stop"
        );
        let beyond = child_page(&scenario, &array.state, u64::MAX, 1).await;
        assert_eq!(beyond.offset, u64::MAX);
        assert!(beyond.children.is_empty());

        for limit in [0, 257] {
            assert!(matches!(
                scenario
                    .handle()
                    .value_children(
                        reference.clone(),
                        uscope::ValueChildQuery { offset: 0, limit },
                    )
                    .await,
                Err(Error::InvalidValueChildPageLimit(actual)) if actual == limit
            ));
        }

        assert!(matches!(
            scenario.resume_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        assert!(matches!(
            scenario
                .handle()
                .value_children(
                    reference,
                    uscope::ValueChildQuery {
                        offset: 0,
                        limit: 1,
                    },
                )
                .await,
            Err(Error::StaleStop)
        ));
        let fresh = dereference_named(&scenario, "array_pointer", 1).await;
        let head = child_page(&scenario, &fresh.state, 0, 1).await;
        assert_signed_state(&head.children[0].state, 20);
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn value_child_capabilities_reject_running_state_before_becoming_stale() {
    let mut scenario = Scenario::new("running value children", Scenario::fixture("spin"));
    scenario.add_breakpoint("main").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let array = scenario
        .operation(
            "inspect spin array",
            scenario.handle().variable("spin_values"),
        )
        .await;
    let reference = available_children(&array.state).clone();
    scenario.remove_all_breakpoints().await;
    let running = scenario.start_resuming().await;
    assert!(matches!(
        scenario
            .handle()
            .value_children(
                reference.clone(),
                uscope::ValueChildQuery {
                    offset: 0,
                    limit: 1,
                },
            )
            .await,
        Err(Error::NotStopped)
    ));
    assert_eq!(scenario.handle().pause().await.unwrap(), StopReason::Pause);
    assert_eq!(running.await.unwrap().unwrap(), StopReason::Pause);
    assert!(matches!(
        scenario
            .handle()
            .value_children(
                reference,
                uscope::ValueChildQuery {
                    offset: 0,
                    limit: 1,
                },
            )
            .await,
        Err(Error::StaleStop)
    ));
    scenario.shutdown().await;
}

#[tokio::test]
async fn dereference_capabilities_are_invalidated_by_the_next_stop() {
    let mut scenario = Scenario::new(
        "stale pointer capability",
        Scenario::fixture("variables-gcc-o0"),
    );
    scenario.add_source_breakpoint("variables.c", 68).await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let pointer = scenario
        .operation(
            "inspect first pointer",
            scenario.handle().variable("pointer"),
        )
        .await;
    let reference = match pointer.state {
        VariableState::Available {
            dereference: uscope::DereferenceState::Available(reference),
            ..
        } => reference,
        state => panic!("pointer was not dereferenceable: {state:?}"),
    };
    let first = scenario
        .operation(
            "first repeated dereference",
            scenario.handle().dereference(reference.clone()),
        )
        .await;
    let repeated = scenario
        .operation(
            "second repeated dereference",
            scenario.handle().dereference(reference.clone()),
        )
        .await;
    assert_eq!(repeated, first);
    assert_dereferenced_scalar(&first, 42, "repeated capability");
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    assert!(matches!(
        scenario.handle().dereference(reference).await,
        Err(Error::StaleStop)
    ));
    let fresh = dereference_named(&scenario, "pointer", 1).await;
    assert_dereferenced_scalar(&fresh, 42, "fresh capability");
    scenario.shutdown().await;
}

#[tokio::test]
async fn dereference_capabilities_reject_running_state_before_becoming_stale() {
    let mut scenario = Scenario::new("running pointer capability", Scenario::fixture("spin"));
    scenario.add_breakpoint("main").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let pointer = scenario
        .operation(
            "inspect spin pointer",
            scenario.handle().variable("spin_pointer"),
        )
        .await;
    let reference = match pointer.state {
        VariableState::Available {
            dereference: uscope::DereferenceState::Available(reference),
            ..
        } => reference,
        state => panic!("spin pointer was not dereferenceable: {state:?}"),
    };
    scenario.remove_all_breakpoints().await;
    let running = scenario.start_resuming().await;
    assert!(matches!(
        scenario.handle().dereference(reference.clone()).await,
        Err(Error::NotStopped)
    ));
    assert_eq!(scenario.handle().pause().await.unwrap(), StopReason::Pause);
    assert_eq!(running.await.unwrap().unwrap(), StopReason::Pause);
    assert!(matches!(
        scenario.handle().dereference(reference).await,
        Err(Error::StaleStop)
    ));
    scenario.shutdown().await;
}

#[tokio::test]
async fn optimized_implicit_pointer_chains_reconstruct_the_referent_without_an_address() {
    let fixture = "variables-gcc-o2";
    let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
    scenario.add_source_breakpoint("variables.c", 80).await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    let pointer_pointer = scenario
        .operation(
            "inspect implicit pointer",
            scenario.handle().variable("pointer_pointer"),
        )
        .await;
    assert!(
        matches!(
            pointer_pointer.state,
            VariableState::Available {
                source: uscope::VariableValueSource::ImplicitPointer,
                raw: None,
                dereference: uscope::DereferenceState::Available(_),
                ..
            }
        ) && matches!(
            available_value(&pointer_pointer.state),
            uscope::VariableValue::ImplicitPointer
        ),
        "{pointer_pointer:?}"
    );

    let pointee = dereference_named(&scenario, "pointer_pointer", 2).await;
    assert_dereferenced_scalar(&pointee, 42, fixture);
    let atomic_pointee = scenario
        .operation(
            "inspect through implicit pointer chain atomically",
            scenario
                .handle()
                .inspect(parsed_value_expression("**pointer_pointer")),
        )
        .await;
    assert_inspected_signed(&atomic_pointee, 42, fixture);
    scenario.shutdown().await;

    let mut offset = Scenario::new(
        "nonzero implicit pointer offset",
        Scenario::fixture(fixture),
    );
    offset.add_source_breakpoint("variables.c", 87).await;
    assert!(matches!(
        offset.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let byte_pointer = offset
        .operation(
            "inspect offset implicit pointer",
            offset.handle().variable("byte_pointer"),
        )
        .await;
    assert!(
        matches!(
            byte_pointer.state,
            VariableState::Available {
                source: uscope::VariableValueSource::ImplicitPointer,
                raw: None,
                dereference: uscope::DereferenceState::Available(_),
                ..
            }
        ) && matches!(
            available_value(&byte_pointer.state),
            uscope::VariableValue::ImplicitPointer
        ),
        "{byte_pointer:?}"
    );
    let byte = dereference_named(&offset, "byte_pointer", 1).await;
    assert_eq!(
        available_value(&byte.state),
        &uscope::VariableValue::Scalar(ScalarValue::Unsigned(42)),
        "{byte:?}"
    );
    let atomic_byte = offset
        .operation(
            "inspect through offset implicit pointer atomically",
            offset
                .handle()
                .inspect(parsed_value_expression("*byte_pointer")),
        )
        .await;
    assert!(
        matches!(
            available_value(&atomic_byte.state),
            uscope::VariableValue::Scalar(ScalarValue::Unsigned(42))
        ),
        "{atomic_byte:?}"
    );
    offset.shutdown().await;
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

#[tokio::test]
async fn stack_scalar_parameters_are_read_through_the_public_scenario_path() {
    for fixture in [
        "variables-parameters-gcc-o0",
        "variables-parameters-clang-o0",
    ] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
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
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
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
async fn source_step_into_stops_after_the_physical_prologue_with_readable_parameters() {
    for case in entry_boundary_cases() {
        let mut scenario = Scenario::new(
            format!("step entry boundary {}", case.fixture),
            Scenario::fixture(case.fixture),
        );
        let expected = expected_physical_entry(&scenario, &case);
        scenario
            .add_source_breakpoint(case.source, case.call_line)
            .await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let mut entered = false;
        for _ in 0..32 {
            assert_eq!(
                scenario.step_to_stop(StepKind::IntoSource).await,
                StopReason::Step {
                    kind: StepKind::IntoSource
                },
                "{} step-in terminated before entering {}",
                case.fixture,
                case.function
            );
            let location = scenario
                .operation("step-in location", scenario.handle().current_location())
                .await;
            if location
                .image
                .function
                .as_ref()
                .is_some_and(|function| function.name.as_ref() == case.function)
            {
                entered = true;
                assert_entry_stop(&scenario, &case, expected).await;
                break;
            }
        }
        assert!(
            entered,
            "{} did not enter {} within the source-step budget",
            case.fixture, case.function
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn boundary_fixture_preserves_inline_step_and_next_semantics() {
    for fixture in ["stepping-boundaries-gcc-o2", "stepping-boundaries-clang-o2"] {
        let mut step = Scenario::new(format!("inline step {fixture}"), Scenario::fixture(fixture));
        step.add_breakpoint("main").await;
        assert!(matches!(
            step.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let physical = step
            .operation("physical boundary caller", step.handle().current_location())
            .await;

        assert_eq!(
            step.step_to_stop(StepKind::IntoSource).await,
            StopReason::Step {
                kind: StepKind::IntoSource
            },
            "{fixture}"
        );
        let inlined = step
            .operation("inline boundary location", step.handle().current_location())
            .await;
        assert_eq!(
            inlined.image.physical_instance, physical.image.physical_instance,
            "{fixture} entered a physical callee instead of a logical inline frame"
        );
        assert_eq!(
            inlined
                .image
                .function
                .as_ref()
                .map(|function| function.name.as_ref()),
            Some("inline_adjust"),
            "{fixture}"
        );
        assert_eq!(
            inlined
                .image
                .source
                .as_ref()
                .map(|source| source.line.get()),
            Some(24),
            "{fixture}: {inlined:?}"
        );
        let sink = fixture_symbol_address(&step, &inlined, "boundary_sink");
        assert_eq!(
            boundary_sink_value(&step, sink).await,
            0,
            "{fixture} executed inline user work before its entry stop"
        );
        step.shutdown().await;

        let mut next = Scenario::new(format!("inline next {fixture}"), Scenario::fixture(fixture));
        next.add_breakpoint("main").await;
        next.run_to_stop().await;
        advance_to_boundary_inline_call(&mut next, fixture).await;

        assert_eq!(
            next.step_to_stop(StepKind::OverSource).await,
            StopReason::Step {
                kind: StepKind::OverSource
            },
            "{fixture}"
        );
        let after = next
            .operation(
                "after inline boundary next",
                next.handle().current_location(),
            )
            .await;
        assert_eq!(
            after
                .image
                .function
                .as_ref()
                .map(|function| function.name.as_ref()),
            Some("main"),
            "{fixture}"
        );
        assert_eq!(
            after.image.source.as_ref().map(|source| source.line.get()),
            Some(31),
            "{fixture}"
        );
        next.shutdown().await;
    }
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

#[tokio::test]
async fn clang_o0_inline_steps_cover_entry_body_return_caller_and_exit() {
    let fixture = "stepping-boundaries-clang-o0";
    let (mut scenario, _) =
        launch_boundary_scenario("Clang O0 inline lifecycle".into(), fixture).await;
    let inlined = boundary_source_step(
        &mut scenario,
        StepKind::IntoSource,
        "first inline statement",
    )
    .await;
    assert_eq!(boundary_function(&inlined), Some("inline_adjust"));
    assert_eq!(boundary_line(&inlined), Some(24));
    let physical = inlined.image.physical_instance;
    let sink = fixture_symbol_address(&scenario, &inlined, "boundary_sink");
    assert_eq!(boundary_sink_value(&scenario, sink).await, 0);

    for (expected_line, expected_sink) in [(25, 0), (26, 6)] {
        let location =
            boundary_source_step(&mut scenario, StepKind::OverSource, "next inline statement")
                .await;
        assert_eq!(boundary_line(&location), Some(expected_line));
        assert_eq!(boundary_function(&location), Some("inline_adjust"));
        assert_eq!(location.image.physical_instance, physical);
        assert_eq!(
            boundary_sink_value(&scenario, sink).await,
            expected_sink,
            "inline statement {expected_line} has the wrong stop-before side effects"
        );
    }

    let caller = boundary_source_step(
        &mut scenario,
        StepKind::OverSource,
        "logical caller after inline return",
    )
    .await;
    assert_eq!(caller.image.physical_instance, physical);
    assert_eq!(boundary_function(&caller), Some("main"));
    assert_eq!(boundary_line(&caller), Some(30));

    let following_call = boundary_source_step(
        &mut scenario,
        StepKind::OverSource,
        "statement following inline call",
    )
    .await;
    assert_eq!(boundary_function(&following_call), Some("main"));
    assert_eq!(boundary_line(&following_call), Some(31));

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

#[tokio::test]
async fn next_walks_the_entire_boundary_fixture_to_a_normal_exit() {
    for fixture in [
        "stepping-boundaries-gcc-o0",
        "stepping-boundaries-clang-o0",
        "stepping-boundaries-gcc-o2",
        "stepping-boundaries-clang-o2",
    ] {
        let (mut scenario, _) =
            launch_boundary_scenario(format!("full next walk {fixture}"), fixture).await;
        let call = advance_to_boundary_inline_call(&mut scenario, fixture).await;
        let sink = fixture_symbol_address(&scenario, &call, "boundary_sink");

        for (expected_line, expected_sink) in [(31, 6), (33, 11), (34, 22), (35, 4)] {
            let mut reached = false;
            for _ in 0..3 {
                let location = boundary_source_step(
                    &mut scenario,
                    StepKind::OverSource,
                    "full next walk location",
                )
                .await;
                assert_eq!(
                    boundary_function(&location),
                    Some("main"),
                    "{fixture}: {location:?}"
                );
                let line = boundary_line(&location).expect("main next stop has source");
                assert!(
                    line <= expected_line,
                    "{fixture} skipped past expected line {expected_line} to {line}"
                );
                if line == expected_line {
                    reached = true;
                    break;
                }
            }
            assert!(
                reached,
                "{fixture} did not reach main line {expected_line} within the step budget"
            );
            assert_eq!(
                boundary_sink_value(&scenario, sink).await,
                expected_sink,
                "{fixture} did not execute the expected callee before main line {expected_line}"
            );
        }

        let mut exit = scenario.step_to_stop(StepKind::OverSource).await;
        if matches!(exit, StopReason::Step { .. }) {
            let closing_brace = scenario
                .operation(
                    "optional closing-brace stop",
                    scenario.handle().current_location(),
                )
                .await;
            assert_eq!(
                boundary_line(&closing_brace),
                Some(36),
                "{fixture} added an unexpected stop after main's return"
            );
            exit = scenario.step_to_stop(StepKind::OverSource).await;
        }
        assert_eq!(
            exit,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture} did not preserve the inferior's normal exit while next completed"
        );
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
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

#[tokio::test]
async fn finish_distinguishes_inline_and_physical_frames_across_the_boundary_fixture() {
    for fixture in [
        "stepping-boundaries-gcc-o0",
        "stepping-boundaries-clang-o0",
        "stepping-boundaries-gcc-o2",
        "stepping-boundaries-clang-o2",
    ] {
        let (mut scenario, main) =
            launch_boundary_scenario(format!("inline and physical finish {fixture}"), fixture)
                .await;
        let main_physical = main.image.physical_instance;

        let inlined =
            boundary_source_step(&mut scenario, StepKind::IntoSource, "inline activation").await;
        assert_eq!(
            boundary_function(&inlined),
            Some("inline_adjust"),
            "{fixture}: {inlined:?}"
        );
        assert_eq!(inlined.image.physical_instance, main_physical);

        let after_inline =
            boundary_source_step(&mut scenario, StepKind::Out, "caller after inline finish").await;
        assert_eq!(after_inline.image.physical_instance, main_physical);
        assert_eq!(boundary_function(&after_inline), Some("main"));
        let after_inline_line = boundary_line(&after_inline).expect("inline finish has source");
        assert!(
            (30..=31).contains(&after_inline_line),
            "{fixture} finished inline_adjust at unexpected line {after_inline_line}"
        );
        advance_boundary_to_line(&mut scenario, fixture, 31).await;
        let sink = fixture_symbol_address(&scenario, &after_inline, "boundary_sink");

        for case in [
            (31, "marked_returns", 11, 33),
            (33, "marked_returns", 22, 34),
            (34, "no_prologue", 4, 35),
        ] {
            finish_boundary_physical_call(&mut scenario, fixture, main_physical, sink, case).await;
        }

        advance_boundary_to_line(&mut scenario, fixture, 35).await;
        assert_eq!(
            scenario.step_to_stop(StepKind::Out).await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture} top-level finish did not preserve normal process exit"
        );
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

#[tokio::test]
async fn rust_o0_inline_steps_cross_source_holes_and_return_to_the_caller() {
    let fixture = "stepping-boundaries-rust-o0";
    let (mut scenario, _) =
        launch_boundary_scenario("Rust O0 inline lifecycle".into(), fixture).await;
    advance_boundary_to_line(&mut scenario, fixture, 39).await;

    let inlined = boundary_source_step(
        &mut scenario,
        StepKind::IntoSource,
        "first Rust inline statement",
    )
    .await;
    assert_eq!(boundary_function(&inlined), Some("inline_adjust"));
    assert_eq!(boundary_line(&inlined), Some(31));
    let physical = inlined.image.physical_instance;
    let sink = fixture_symbol_address(&scenario, &inlined, "RUST_BOUNDARY_SINK");
    assert_eq!(boundary_sink_value(&scenario, sink).await, 0);

    for (expected_line, expected_sink) in [(32, 0), (33, 6)] {
        let location = boundary_source_step(
            &mut scenario,
            StepKind::OverSource,
            "next Rust inline statement",
        )
        .await;
        assert_eq!(boundary_function(&location), Some("inline_adjust"));
        assert_eq!(boundary_line(&location), Some(expected_line));
        assert_eq!(location.image.physical_instance, physical);
        assert_eq!(boundary_sink_value(&scenario, sink).await, expected_sink);
    }

    let caller = boundary_source_step(
        &mut scenario,
        StepKind::OverSource,
        "Rust caller after inline return",
    )
    .await;
    assert_eq!(boundary_function(&caller), Some("main"));
    assert_eq!(boundary_line(&caller), Some(39));
    assert_eq!(caller.image.physical_instance, physical);

    let following_call = boundary_source_step(
        &mut scenario,
        StepKind::OverSource,
        "Rust statement following inline call",
    )
    .await;
    assert_eq!(boundary_function(&following_call), Some("main"));
    assert_eq!(boundary_line(&following_call), Some(40));

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

#[tokio::test]
async fn rust_o2_inline_steps_follow_optimized_statements_and_return_to_the_caller() {
    let fixture = "stepping-boundaries-rust-o2";
    let (mut scenario, _) =
        launch_boundary_scenario("Rust O2 inline lifecycle".into(), fixture).await;
    advance_boundary_to_line(&mut scenario, fixture, 39).await;

    let first = boundary_source_step(
        &mut scenario,
        StepKind::IntoSource,
        "first optimized Rust inline statement",
    )
    .await;
    assert_eq!(boundary_function(&first), Some("inline_adjust"));
    assert_eq!(boundary_line(&first), Some(31));
    let physical = first.image.physical_instance;
    let sink = fixture_symbol_address(&scenario, &first, "RUST_BOUNDARY_SINK");
    assert_eq!(boundary_sink_value(&scenario, sink).await, 0);

    let second = boundary_source_step(
        &mut scenario,
        StepKind::OverSource,
        "second optimized Rust inline statement",
    )
    .await;
    assert_eq!(boundary_function(&second), Some("inline_adjust"));
    assert_eq!(boundary_line(&second), Some(32));
    assert_eq!(second.image.physical_instance, physical);
    assert_eq!(boundary_sink_value(&scenario, sink).await, 0);

    let caller = boundary_source_step(
        &mut scenario,
        StepKind::OverSource,
        "optimized Rust caller after inline return",
    )
    .await;
    assert_eq!(boundary_function(&caller), Some("main"));
    assert_eq!(boundary_line(&caller), Some(40));
    assert_eq!(caller.image.physical_instance, physical);
    assert_eq!(boundary_sink_value(&scenario, sink).await, 6);

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

#[tokio::test]
async fn next_walks_the_entire_rust_boundary_fixture_to_a_normal_exit() {
    for fixture in ["stepping-boundaries-rust-o0", "stepping-boundaries-rust-o2"] {
        let (mut scenario, main) =
            launch_boundary_scenario(format!("full Rust next walk {fixture}"), fixture).await;
        let sink = fixture_symbol_address(&scenario, &main, "RUST_BOUNDARY_SINK");
        let expected: &[(u64, u64)] = if fixture.ends_with("o0") {
            &[
                (39, 0),
                (40, 6),
                (41, 11),
                (42, 11),
                (43, 11),
                (44, 22),
                (45, 4),
            ]
        } else {
            &[(39, 0), (40, 6), (41, 11), (43, 11), (44, 22), (45, 4)]
        };

        for &(expected_line, expected_sink) in expected {
            let location = advance_boundary_to_line(&mut scenario, fixture, expected_line).await;
            assert_eq!(boundary_function(&location), Some("main"));
            assert_eq!(boundary_sink_value(&scenario, sink).await, expected_sink);
        }

        let mut exit = scenario.step_to_stop(StepKind::OverSource).await;
        if matches!(exit, StopReason::Step { .. }) {
            let closing = scenario
                .operation(
                    "optional Rust closing-brace stop",
                    scenario.handle().current_location(),
                )
                .await;
            assert_eq!(boundary_line(&closing), Some(46), "{fixture}");
            exit = scenario.step_to_stop(StepKind::OverSource).await;
        }
        assert_eq!(exit, StopReason::Exited(ExitStatus::Code(0)), "{fixture}");
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

#[tokio::test]
async fn finish_distinguishes_rust_inline_and_physical_frames() {
    for fixture in ["stepping-boundaries-rust-o0", "stepping-boundaries-rust-o2"] {
        let (mut scenario, main) =
            launch_boundary_scenario(format!("Rust finish lifecycle {fixture}"), fixture).await;
        let main_physical = main.image.physical_instance;
        advance_boundary_to_line(&mut scenario, fixture, 39).await;

        let inlined = boundary_source_step(
            &mut scenario,
            StepKind::IntoSource,
            "Rust inline activation",
        )
        .await;
        assert_eq!(boundary_function(&inlined), Some("inline_adjust"));
        assert_eq!(inlined.image.physical_instance, main_physical);
        let returned = boundary_source_step(
            &mut scenario,
            StepKind::Out,
            "caller after Rust inline finish",
        )
        .await;
        assert_eq!(boundary_function(&returned), Some("main"));
        assert_eq!(returned.image.physical_instance, main_physical);
        assert!((39..=40).contains(&boundary_line(&returned).expect("Rust caller source")));

        let sink = fixture_symbol_address(&scenario, &returned, "RUST_BOUNDARY_SINK");
        for case in [
            (40, "marked_returns", 11, 42),
            (43, "marked_returns", 22, 44),
            (44, "no_prologue", 4, 45),
        ] {
            finish_boundary_physical_call(&mut scenario, fixture, main_physical, sink, case).await;
        }

        advance_boundary_to_line(&mut scenario, fixture, 45).await;
        assert_eq!(
            scenario.step_to_stop(StepKind::Out).await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture}"
        );
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

#[tokio::test]
async fn gcc_o2_entry_policy_does_not_execute_a_real_first_statement() {
    let fixture = "stepping-boundaries-gcc-o2";
    let mut scenario = Scenario::new("GCC O2 zero-length prologue", Scenario::fixture(fixture));
    let raw_entry = {
        let image = scenario.handle().module_image();
        let function = image.function_named("no_prologue").expect("no_prologue");
        image
            .instances_for_function(function.id)
            .find(|instance| matches!(instance.kind, CodeInstanceKind::OutOfLine))
            .expect("physical no_prologue")
            .ranges[0]
            .start
    };
    let breakpoint = scenario.add_breakpoint("no_prologue").await;
    assert_eq!(
        single_image_breakpoint_address(&breakpoint),
        raw_entry,
        "the conservative GCC fallback skipped the first store"
    );

    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let location = scenario
        .operation(
            "zero-prologue location",
            scenario.handle().current_location(),
        )
        .await;
    assert_eq!(location.image.address, raw_entry);
    let sink = scenario
        .handle()
        .module_image()
        .symbol_named("boundary_sink")
        .expect("boundary_sink symbol")
        .address;
    let sink = relocate_image_address(sink, &location);
    let word = scenario
        .operation(
            "boundary sink before first instruction",
            scenario.handle().read_word(sink),
        )
        .await;
    assert_eq!(
        u32::try_from(word).expect("boundary sink value fits u32"),
        22,
        "no_prologue's first store executed before its entry stop"
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn next_crosses_each_marked_epilogue_and_completes_in_the_caller() {
    let fixture = "stepping-boundaries-clang-o2";
    let mut scenario = Scenario::new("multiple marked epilogues", Scenario::fixture(fixture));
    let markers = epilogue_markers(&scenario, "marked_returns");
    assert_eq!(
        markers.len(),
        2,
        "fixture must retain two distinct marked return paths"
    );
    scenario
        .add_source_breakpoint("stepping-boundaries.c", 11)
        .await;
    scenario
        .add_source_breakpoint("stepping-boundaries.c", 15)
        .await;

    for return_line in [11, 15] {
        let reason = if return_line == 11 {
            scenario.run_to_stop().await
        } else {
            scenario.resume_to_stop().await
        };
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "did not stop on return line {return_line}: {reason:?}"
        );
        let before = scenario
            .operation(
                "return statement location",
                scenario.handle().current_location(),
            )
            .await;
        assert_eq!(
            before.image.source.as_ref().map(|source| source.line.get()),
            Some(return_line)
        );

        assert_eq!(
            scenario.step_to_stop(StepKind::OverSource).await,
            StopReason::Step {
                kind: StepKind::OverSource
            },
            "next did not complete across return line {return_line}"
        );
        let after = scenario
            .operation("caller after return", scenario.handle().current_location())
            .await;
        assert_eq!(
            after
                .image
                .function
                .as_ref()
                .map(|function| function.name.as_ref()),
            Some("main"),
            "next exposed an epilogue stop for return line {return_line}: {after:?}"
        );
        assert!(
            !markers.contains(&after.image.address),
            "next published compiler epilogue marker {}",
            after.image.address
        );
    }

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn step_uses_each_marked_epilogue_to_complete_in_the_caller() {
    let fixture = "stepping-boundaries-clang-o2";
    let mut scenario = Scenario::new("step through marked epilogues", Scenario::fixture(fixture));
    let markers = epilogue_markers(&scenario, "marked_returns");
    assert_eq!(markers.len(), 2, "fixture boundary contract changed");
    scenario
        .add_source_breakpoint("stepping-boundaries.c", 11)
        .await;
    scenario
        .add_source_breakpoint("stepping-boundaries.c", 15)
        .await;

    for return_line in [11, 15] {
        let reason = if return_line == 11 {
            scenario.run_to_stop().await
        } else {
            scenario.resume_to_stop().await
        };
        assert!(matches!(reason, StopReason::Breakpoint { .. }));

        assert_eq!(
            scenario.step_to_stop(StepKind::IntoSource).await,
            StopReason::Step {
                kind: StepKind::IntoSource
            }
        );
        let after = scenario
            .operation(
                "step caller after return",
                scenario.handle().current_location(),
            )
            .await;
        assert_eq!(
            after
                .image
                .function
                .as_ref()
                .map(|function| function.name.as_ref()),
            Some("main"),
            "step exposed an epilogue stop for return line {return_line}: {after:?}"
        );
        assert!(!markers.contains(&after.image.address));
    }

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn an_explicit_user_breakpoint_at_an_epilogue_marker_remains_visible() {
    let fixture = "stepping-boundaries-clang-o2";
    let mut scenario = Scenario::new("explicit epilogue breakpoint", Scenario::fixture(fixture));
    let marker = *epilogue_markers(&scenario, "marked_returns")
        .iter()
        .max()
        .expect("negative return path marker");
    scenario.add_breakpoint("marked_returns").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let entry = scenario
        .operation(
            "marked function entry",
            scenario.handle().current_location(),
        )
        .await;
    let marker = relocate_image_address(marker, &entry);
    scenario
        .add_breakpoint_spec(uscope::BreakpointSpec::Address(marker))
        .await;

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { address: marker },
        "the internal exit policy hid an explicit user breakpoint"
    );
    scenario.shutdown().await;
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

#[tokio::test]
async fn next_from_an_inline_frame_crosses_a_tail_call_to_the_true_caller() {
    for fixture in ["tail-calls-gcc-o2", "tail-calls-clang-o2"] {
        for (function, inline_function, tail_line, caller_line, expected_sink) in [
            ("outer_tail", "inline_tail", 22, 81, 20),
            ("outer_chain", "inline_chain", 32, 82, 21),
        ] {
            let mut scenario = Scenario::new(
                format!("tail-call next {fixture} {function}"),
                Scenario::fixture(fixture),
            );
            scenario.add_breakpoint(function).await;
            assert!(matches!(
                scenario.run_to_stop().await,
                StopReason::Breakpoint { .. }
            ));
            enter_inline_frame(&mut scenario, fixture, inline_function, tail_line).await;

            let stop =
                boundary_source_step(&mut scenario, StepKind::OverSource, "next across tail call")
                    .await;
            assert_eq!(
                boundary_function(&stop),
                Some("main"),
                "{fixture} {function} next stopped inside the tail-called function: {stop:?}"
            );
            let line = boundary_line(&stop).expect("tail-call next stop has caller source");
            assert!(
                (caller_line..=caller_line + 1).contains(&line),
                "{fixture} {function} completed at unexpected main line {line}"
            );
            let sink = fixture_symbol_address(&scenario, &stop, "tail_sink");
            assert_eq!(
                boundary_sink_value(&scenario, sink).await,
                expected_sink,
                "{fixture} {function} stopped before the tail-called work finished"
            );

            assert_eq!(
                scenario.resume_to_stop().await,
                StopReason::Exited(ExitStatus::Code(0)),
                "{fixture} {function}"
            );
            assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
        }
    }
}

#[tokio::test]
async fn finish_from_an_inline_frame_crosses_its_parents_tail_call() {
    for fixture in ["tail-calls-gcc-o2", "tail-calls-clang-o2"] {
        let mut scenario = Scenario::new(
            format!("tail-call finish {fixture}"),
            Scenario::fixture(fixture),
        );
        scenario.add_breakpoint("outer_tail").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        enter_inline_frame(&mut scenario, fixture, "inline_tail", 21).await;

        let stop =
            boundary_source_step(&mut scenario, StepKind::Out, "finish across tail call").await;
        assert_eq!(
            boundary_function(&stop),
            Some("main"),
            "{fixture} finish stopped inside the tail-called function: {stop:?}"
        );
        let line = boundary_line(&stop).expect("tail-call finish stop has caller source");
        assert!(
            (81..=82).contains(&line),
            "{fixture} finish completed at unexpected main line {line}"
        );
        let sink = fixture_symbol_address(&scenario, &stop, "tail_sink");
        assert_eq!(
            boundary_sink_value(&scenario, sink).await,
            20,
            "{fixture} finish stopped before the tail-called work finished"
        );

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture}"
        );
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

#[tokio::test]
async fn recursive_tail_call_completion_ignores_inner_frames_at_the_shared_return_site() {
    for fixture in ["tail-calls-gcc-o2", "tail-calls-clang-o2"] {
        let mut scenario = Scenario::new(
            format!("recursive tail-call next {fixture}"),
            Scenario::fixture(fixture),
        );
        // The first descend_tail activation (value == 2) tail-calls
        // mutual_tail(1), whose inner recursion returns through the same
        // code address as this step's own return site. Only the outer
        // return, distinguished by the stack pointer, may complete the step.
        scenario.add_breakpoint("descend_tail").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        scenario.remove_all_breakpoints().await;
        enter_inline_frame(&mut scenario, fixture, "inline_descend", 44).await;

        let stop = boundary_source_step(
            &mut scenario,
            StepKind::OverSource,
            "next across recursive tail call",
        )
        .await;
        assert_eq!(
            boundary_function(&stop),
            Some("mutual_tail"),
            "{fixture}: {stop:?}"
        );
        let line = boundary_line(&stop).expect("recursive tail-call stop has caller source");
        assert!(
            (55..=57).contains(&line),
            "{fixture} completed at unexpected mutual_tail line {line}"
        );
        let probe = fixture_symbol_address(&scenario, &stop, "tail_probe");
        assert_eq!(
            boundary_sink_value(&scenario, probe).await,
            1,
            "{fixture} completed in an inner recursive frame instead of the starting caller"
        );

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture}"
        );
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

#[tokio::test]
async fn next_from_an_inline_frame_runs_regular_callees_at_full_speed() {
    for fixture in ["tail-calls-gcc-o2", "tail-calls-clang-o2"] {
        let mut scenario = Scenario::new(
            format!("inline regular-call next {fixture}"),
            Scenario::fixture(fixture),
        );
        scenario.add_breakpoint("outer_over_call").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        enter_inline_frame(&mut scenario, fixture, "inline_over_call", 70).await;

        let stop = boundary_source_step(
            &mut scenario,
            StepKind::OverSource,
            "next over long-running regular call",
        )
        .await;
        assert_eq!(boundary_function(&stop), Some("inline_over_call"));
        assert_eq!(boundary_line(&stop), Some(71));
        let counter = fixture_symbol_address(&scenario, &stop, "tail_counter");
        assert_eq!(
            boundary_sink_value(&scenario, counter).await,
            200_000,
            "{fixture} stopped before the regular callee completed"
        );

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture}"
        );
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

#[tokio::test]
async fn finish_from_an_inline_frame_runs_regular_callees_at_full_speed() {
    for fixture in ["tail-calls-gcc-o2", "tail-calls-clang-o2"] {
        let mut scenario = Scenario::new(
            format!("inline regular-call finish {fixture}"),
            Scenario::fixture(fixture),
        );
        scenario.add_breakpoint("outer_over_call").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        enter_inline_frame(&mut scenario, fixture, "inline_over_call", 70).await;

        let stop = boundary_source_step(
            &mut scenario,
            StepKind::Out,
            "finish through long-running regular call",
        )
        .await;
        assert_eq!(boundary_function(&stop), Some("outer_over_call"));
        let line = boundary_line(&stop).expect("regular-call finish stop has parent source");
        assert!(
            (77..=78).contains(&line),
            "{fixture} finish completed at unexpected outer_over_call line {line}"
        );
        let counter = fixture_symbol_address(&scenario, &stop, "tail_counter");
        assert_eq!(
            boundary_sink_value(&scenario, counter).await,
            200_000,
            "{fixture} stopped before the regular callee completed"
        );

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture}"
        );
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
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

#[tokio::test]
async fn global_catalog_normalizes_compiler_qualification_and_optimized_storage() {
    for (fixture, expected) in [
        ("globals-c-gcc-o0", &["external_value", "duplicate"][..]),
        (
            "globals-cpp-gcc-o0",
            &[
                "fixture::alpha::duplicate",
                "fixture::Holder::member",
                "fixture::Holder::constexpr_member",
            ][..],
        ),
        (
            "globals-cpp-clang-o0",
            &[
                "fixture::alpha::duplicate",
                "fixture::Holder::member",
                "fixture::Holder::constexpr_member",
            ][..],
        ),
        (
            "globals-rust-o0",
            &[
                "globals::ROOT_IMMUTABLE",
                "globals::alpha::DUPLICATE",
                "globals::beta::DUPLICATE",
            ][..],
        ),
        (
            "globals-go-o0",
            &["main.packageValue", "main.packageMutable"][..],
        ),
        (
            "globals-zig-o0",
            &[
                "globals.root_value",
                "globals.Alpha.duplicate",
                "globals.Beta.duplicate",
            ][..],
        ),
    ] {
        let debugger = Debugger::new(Scenario::fixture(fixture)).expect("load global catalog");
        let handle = debugger.handle();
        let image = handle.module_image();
        for qualified in expected {
            catalog_global(image, qualified);
        }
        for global in image.globals() {
            if let uscope::GlobalVariableType::Resolved(type_info) = &global.type_info {
                assert_eq!(
                    image.type_info(type_info.reference),
                    Some(type_info),
                    "{fixture}: global {} published type metadata that disagrees with its type graph node",
                    global.qualified_name
                );
            }
        }
        debugger
            .shutdown()
            .await
            .expect("shut down catalog debugger");
    }

    for fixture in ["globals-rust-o2", "globals-zig-o2"] {
        let debugger = Debugger::new(Scenario::fixture(fixture)).expect("load optimized catalog");
        let handle = debugger.handle();
        let global = handle
            .module_image()
            .globals()
            .iter()
            .find(|global| matches!(global.name.as_ref(), "OPTIMIZED_AWAY" | "root_constant"))
            .unwrap_or_else(|| panic!("{fixture} missing optimized global"));
        assert!(matches!(
            global.type_info,
            uscope::GlobalVariableType::Resolved(_)
        ));
        debugger
            .shutdown()
            .await
            .expect("shut down catalog debugger");
    }
}

#[tokio::test]
async fn global_catalog_listing_is_filtered_bounded_and_deterministic() {
    let debugger = Debugger::new(Scenario::fixture("globals-go-o0")).expect("load Go catalog");
    let handle = debugger.handle();
    let first = handle
        .globals(uscope::GlobalVariableQuery {
            filter: Some("main.package".to_owned()),
            offset: 0,
            limit: 1,
        })
        .await
        .expect("first global page");
    assert_eq!(first.offset, 0);
    assert_eq!(first.total, 7);
    assert_eq!(first.variables.len(), 1);
    assert!(first.variables[0].module.is_none());
    let second = handle
        .globals(uscope::GlobalVariableQuery {
            filter: Some("main.package".to_owned()),
            offset: 1,
            limit: 1,
        })
        .await
        .expect("second global page");
    assert_eq!(second.total, first.total);
    assert_eq!(second.variables.len(), 1);
    assert!(
        first.variables[0].variable.qualified_name < second.variables[0].variable.qualified_name
    );
    assert!(matches!(
        handle
            .globals(uscope::GlobalVariableQuery {
                filter: None,
                offset: 0,
                limit: 0,
            })
            .await,
        Err(Error::InvalidGlobalPageLimit(0))
    ));
    drop(handle);
    debugger
        .shutdown()
        .await
        .expect("shut down catalog debugger");
}

#[tokio::test]
async fn c_globals_cover_local_shadowing_collisions_relocation_and_optimization() {
    for fixture in [
        "globals-c-gcc-o0",
        "globals-c-clang-o0",
        "globals-c-gcc-o2",
        "globals-c-clang-o2",
        "globals-c-gcc-nopie",
    ] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_source_breakpoint("main.c", 9).await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let shadow = scenario
            .operation(
                "inspect local shadow",
                scenario.handle().variable("external_value"),
            )
            .await;
        assert_eq!(shadow.kind, VariableKind::Local);
        assert_variable_value(&shadow, ScalarValue::Signed(999));

        let external = catalog_global(scenario.handle().module_image(), "external_value");
        let external = scenario
            .operation(
                "inspect exact external global",
                scenario.handle().main_global(external.id),
            )
            .await;
        assert_eq!(external.kind, VariableKind::Global);
        assert!(external.global.is_some());
        assert_variable_value(&external, ScalarValue::Signed(101));

        let pointer = scenario
            .operation(
                "inspect global pointer",
                scenario.handle().variable("external_pointer"),
            )
            .await;
        let reference = match pointer.state {
            VariableState::Available {
                dereference: uscope::DereferenceState::Available(reference),
                ..
            } => reference,
            state => panic!("{fixture} global pointer was unavailable: {state:?}"),
        };
        let dereferenced = scenario
            .operation(
                "dereference global pointer",
                scenario.handle().dereference(reference),
            )
            .await;
        assert_dereferenced_scalar(&dereferenced, 101, fixture);

        let one = scenario
            .operation(
                "inspect first file static",
                scenario.handle().variable("one.c::duplicate"),
            )
            .await;
        let two = scenario
            .operation(
                "inspect second file static",
                scenario.handle().variable("two.c::duplicate"),
            )
            .await;
        assert_variable_value(&one, ScalarValue::Signed(201));
        assert_variable_value(&two, ScalarValue::Signed(202));
        assert!(matches!(
            scenario.handle().variable("duplicate").await,
            Err(Error::AmbiguousGlobalVariable { .. })
        ));

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn cpp_globals_resolve_namespaces_static_members_specifications_and_constants() {
    for fixture in [
        "globals-cpp-gcc-o0",
        "globals-cpp-clang-o0",
        "globals-cpp-gcc-o2",
        "globals-cpp-clang-o2",
    ] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_breakpoint("inspect_globals").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        for (name, expected) in [
            ("fixture::alpha::duplicate", 121),
            ("fixture::beta::duplicate", 122),
            ("fixture::Holder::member", 131),
            ("fixture::Holder::inline_member", 132),
            ("fixture::Holder::constexpr_member", 133),
            ("fixture::Holder::negative_constexpr_member", -123),
            ("fixture::{anonymous}::anonymous_value", 123),
        ] {
            let variable = scenario
                .operation(
                    "inspect qualified C++ global",
                    scenario.handle().variable(name),
                )
                .await;
            assert_variable_value(&variable, ScalarValue::Signed(expected));
        }
        assert!(matches!(
            scenario.handle().variable("duplicate").await,
            Err(Error::AmbiguousGlobalVariable { .. })
        ));

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn rust_globals_preserve_module_qualification_and_honest_optimized_unavailability() {
    let mut scenario = Scenario::new("Rust globals O0", Scenario::fixture("globals-rust-o0"));
    scenario.add_breakpoint("inspect_globals").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    for (name, expected) in [
        ("globals::ROOT_IMMUTABLE", 141),
        ("globals::ROOT_MUTABLE", 142),
        ("globals::alpha::DUPLICATE", 151),
        ("globals::beta::DUPLICATE", 152),
    ] {
        let variable = scenario
            .operation("inspect Rust global", scenario.handle().variable(name))
            .await;
        assert_variable_value(&variable, ScalarValue::Signed(expected));
    }
    assert!(matches!(
        scenario.handle().variable("DUPLICATE").await,
        Err(Error::AmbiguousGlobalVariable { .. })
    ));
    assert_dereferenced_scalar(
        &dereference_named(&scenario, "globals::ROOT_POINTER", 1).await,
        157,
        "globals-rust-o0",
    );
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;

    let mut optimized = Scenario::new("Rust globals O2", Scenario::fixture("globals-rust-o2"));
    optimized.add_breakpoint("inspect_globals").await;
    assert!(matches!(
        optimized.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let root = optimized
        .operation(
            "inspect optimized Rust global",
            optimized.handle().variable("globals::ROOT_IMMUTABLE"),
        )
        .await;
    assert!(matches!(
        root.state,
        VariableState::Unavailable(
            uscope::VariableUnavailableReason::OptimizedOut
                | uscope::VariableUnavailableReason::Other(_),
        )
    ));
    assert_dereferenced_scalar(
        &dereference_named(&optimized, "globals::ROOT_POINTER", 1).await,
        157,
        "globals-rust-o2",
    );
    assert_eq!(
        optimized.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    optimized.shutdown().await;
}

#[tokio::test]
async fn go_package_globals_are_printable_without_source_stepping() {
    let fixture = "globals-go-o0";
    let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
    scenario.add_breakpoint("main.inspectGlobals").await;
    run_go_to_breakpoint(&mut scenario, fixture).await;
    for (name, expected) in [
        ("main.packageValue", ScalarValue::Signed(161)),
        ("main.packageMutable", ScalarValue::Signed(162)),
    ] {
        let variable = scenario
            .operation(
                "inspect Go package global",
                scenario.handle().variable(name),
            )
            .await;
        assert_variable_value(&variable, expected);
    }
    assert_dereferenced_scalar(
        &dereference_named(&scenario, "main.packagePointer", 1).await,
        162,
        fixture,
    );
    assert_dereferenced_scalar(
        &dereference_named(&scenario, "main.packagePointerPointer", 2).await,
        162,
        fixture,
    );
    let nil = scenario
        .operation(
            "inspect Go package nil pointer",
            scenario.handle().variable("main.packageNil"),
        )
        .await;
    assert!(matches!(
        nil.state,
        VariableState::Available {
            dereference: uscope::DereferenceState::Unavailable {
                reason: uscope::DereferenceUnavailableReason::Null,
                ..
            },
            ..
        }
    ));
    let pair = dereference_named(&scenario, "main.packagePairPointer", 1).await;
    assert_dereferenced_record(&scenario, &pair, 2, fixture).await;
    resume_go_to_exit(&mut scenario, fixture).await;
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));

    let debugger =
        Debugger::new(Scenario::fixture("globals-go-o2")).expect("load optimized Go globals");
    let handle = debugger.handle();
    catalog_global(handle.module_image(), "main.packageValue");
    let pointer = catalog_global(handle.module_image(), "main.packagePointer");
    assert!(matches!(
        &pointer.type_info,
        uscope::GlobalVariableType::Resolved(uscope::TypeInfo {
            kind: uscope::TypeKind::Pointer { .. },
            ..
        })
    ));
    debugger
        .shutdown()
        .await
        .expect("shut down optimized Go debugger");
}

#[tokio::test]
async fn zig_globals_cover_containers_constants_pie_and_optimized_storage() {
    for fixture in ["globals-zig-o0", "globals-zig-nopie"] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_breakpoint("inspectGlobals").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        for (name, expected) in [
            ("globals.root_value", 171),
            ("globals.root_constant", 172),
            ("globals.Alpha.duplicate", 181),
            ("globals.Alpha.constant", 182),
            ("globals.Beta.duplicate", 183),
        ] {
            let variable = scenario
                .operation("inspect Zig global", scenario.handle().variable(name))
                .await;
            assert_variable_value(&variable, ScalarValue::Signed(expected));
        }
        assert!(matches!(
            scenario.handle().variable("duplicate").await,
            Err(Error::AmbiguousGlobalVariable { .. })
        ));
        assert_dereferenced_scalar(
            &dereference_named(&scenario, "globals.root_pointer", 1).await,
            184,
            fixture,
        );
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        scenario.shutdown().await;
    }

    let mut optimized = Scenario::new("Zig globals O2", Scenario::fixture("globals-zig-o2"));
    optimized.add_breakpoint("inspectGlobals").await;
    assert!(matches!(
        optimized.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let constant = optimized
        .operation(
            "inspect optimized Zig global",
            optimized.handle().variable("globals.root_constant"),
        )
        .await;
    assert!(matches!(constant.state, VariableState::Unavailable(_)));
    let pointer = optimized
        .operation(
            "inspect optimized Zig pointer global",
            optimized.handle().variable("globals.root_pointer"),
        )
        .await;
    assert!(matches!(pointer.state, VariableState::Unavailable(_)));
    assert_eq!(
        optimized.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    optimized.shutdown().await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one lifecycle scenario must retain identities across load, unload, and reload"
)]
async fn shared_library_globals_track_load_unload_reload_and_stale_identity() {
    let mut scenario = Scenario::new("shared globals", Scenario::fixture("globals-shared"));
    assert!(matches!(
        scenario.handle().loaded_modules().await,
        Err(Error::NotRunning)
    ));
    scenario.add_breakpoint("after_load").await;
    scenario.add_breakpoint("after_unload").await;
    scenario.add_breakpoint("after_reload").await;
    let mut events = scenario.handle().subscribe();

    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let Err(Error::AmbiguousLoadedGlobalVariable {
        selector,
        candidates,
    }) = scenario.handle().variable("module_collision").await
    else {
        panic!("same-name globals across modules must be ambiguous");
    };
    assert_eq!(selector, "module_collision");
    assert_eq!(candidates.len(), 2);
    let loaded = scenario
        .operation(
            "list loaded DSO globals",
            scenario.handle().globals(uscope::GlobalVariableQuery {
                filter: Some("dso_".to_owned()),
                ..uscope::GlobalVariableQuery::default()
            }),
        )
        .await;
    let external = loaded
        .variables
        .iter()
        .find(|entry| entry.variable.name.as_ref() == "dso_external")
        .expect("DSO external global");
    let first_module = external.module.expect("DSO is loaded");
    let first_reference = uscope::GlobalVariableReference {
        module: first_module.id,
        image: external.image,
        variable: external.variable.id,
    };
    let value = scenario
        .operation(
            "inspect DSO external global",
            scenario.handle().loaded_global(first_reference),
        )
        .await;
    assert_variable_value(&value, ScalarValue::Signed(211));
    let cross_module_pointer =
        catalog_global(scenario.handle().module_image(), "cross_module_pointer");
    let cross_module_pointer = scenario
        .operation(
            "inspect main-image pointer into DSO",
            scenario.handle().main_global(cross_module_pointer.id),
        )
        .await;
    let cross_module_reference = match cross_module_pointer.state {
        VariableState::Available {
            dereference: uscope::DereferenceState::Available(reference),
            ..
        } => reference,
        state => panic!("cross-module pointer was unavailable: {state:?}"),
    };
    let cross_module_referent = scenario
        .operation(
            "dereference main-image pointer into DSO",
            scenario.handle().dereference(cross_module_reference),
        )
        .await;
    assert_dereferenced_scalar(&cross_module_referent, 211, "globals-shared");
    let dso_pointer = loaded
        .variables
        .iter()
        .find(|entry| entry.variable.name.as_ref() == "dso_pointer")
        .expect("DSO pointer global");
    let dso_pointer_module = dso_pointer.module.expect("pointer DSO is loaded");
    let dso_pointer = scenario
        .operation(
            "inspect DSO pointer global",
            scenario
                .handle()
                .loaded_global(uscope::GlobalVariableReference {
                    module: dso_pointer_module.id,
                    image: dso_pointer.image,
                    variable: dso_pointer.variable.id,
                }),
        )
        .await;
    let reference = match dso_pointer.state {
        VariableState::Available {
            dereference: uscope::DereferenceState::Available(reference),
            ..
        } => reference,
        state => panic!("DSO pointer global was unavailable: {state:?}"),
    };
    let dso_referent = scenario
        .operation(
            "dereference DSO pointer global",
            scenario.handle().dereference(reference),
        )
        .await;
    assert_dereferenced_scalar(&dso_referent, 211, "globals-shared");
    let dso_tls = loaded
        .variables
        .iter()
        .find(|entry| entry.variable.name.as_ref() == "dso_tls")
        .expect("DSO TLS global");
    let dso_tls_module = dso_tls.module.expect("TLS DSO is loaded");
    let tls_value = scenario
        .operation(
            "inspect dynamically loaded TLS global",
            scenario
                .handle()
                .loaded_global(uscope::GlobalVariableReference {
                    module: dso_tls_module.id,
                    image: dso_tls.image,
                    variable: dso_tls.variable.id,
                }),
        )
        .await;
    assert_variable_value(&tls_value, ScalarValue::Signed(213));
    let mut saw_load = false;
    while let Ok(event) = events.try_recv() {
        saw_load |= matches!(
            event,
            uscope::DebuggerEvent::ModuleLoaded { module, .. }
                if module.path.ends_with("libglobals.so")
        );
    }
    assert!(saw_load);

    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    assert!(matches!(
        scenario.handle().loaded_global(first_reference).await,
        Err(Error::ModuleNotLoaded(id)) if id == first_module.id
    ));
    let mut saw_unload = false;
    while let Ok(event) = events.try_recv() {
        saw_unload |= matches!(
            event,
            uscope::DebuggerEvent::ModuleUnloaded { module, .. }
                if module.module.id == first_module.id
        );
    }
    assert!(saw_unload);

    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let reloaded = scenario
        .operation(
            "list reloaded DSO globals",
            scenario.handle().globals(uscope::GlobalVariableQuery {
                filter: Some("dso_external".to_owned()),
                ..uscope::GlobalVariableQuery::default()
            }),
        )
        .await;
    let reloaded = reloaded.variables.first().expect("reloaded DSO global");
    let reloaded_module = reloaded.module.expect("DSO was reloaded");
    assert_ne!(reloaded_module.id, first_module.id);
    assert_ne!(reloaded.image, first_reference.image);
    let reloaded_value = scenario
        .operation(
            "inspect reloaded DSO global",
            scenario
                .handle()
                .loaded_global(uscope::GlobalVariableReference {
                    module: reloaded_module.id,
                    image: reloaded.image,
                    variable: reloaded.variable.id,
                }),
        )
        .await;
    assert_variable_value(&reloaded_value, ScalarValue::Signed(211));

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    assert!(matches!(
        scenario.handle().loaded_modules().await,
        Err(Error::NotRunning)
    ));

    scenario.remove_all_breakpoints().await;
    scenario.add_breakpoint("after_load").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let second_run = scenario
        .operation(
            "list second-run modules",
            scenario.handle().loaded_modules(),
        )
        .await;
    let second_run_dso = second_run
        .modules
        .iter()
        .filter(|module| {
            module
                .path
                .file_name()
                .is_some_and(|name| name == "libglobals.so")
        })
        .collect::<Vec<_>>();
    assert_eq!(second_run_dso.len(), 1, "{second_run:?}");
    assert_ne!(second_run_dso[0].module.id, reloaded_module.id);
    scenario.remove_all_breakpoints().await;
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

#[tokio::test]
async fn tls_globals_resolve_per_selected_thread_for_gcc_and_clang() {
    for fixture in ["globals-tls-gcc", "globals-tls-clang"] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
        scenario.add_breakpoint("tls_stop").await;
        scenario.add_breakpoint("tls_after_join").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let global = catalog_global(scenario.handle().module_image(), "tls_value").id;
        let pointer = catalog_global(scenario.handle().module_image(), "tls_pointer").id;
        let snapshot = scenario.snapshot().await;
        assert_eq!(snapshot.threads.len(), 3, "{fixture}: {snapshot:?}");
        let mut values = Vec::new();
        let mut thread_references = Vec::new();
        for thread in snapshot.threads.iter() {
            scenario
                .operation(
                    "select TLS thread",
                    scenario.handle().select_thread(thread.id),
                )
                .await;
            let variable = scenario
                .operation(
                    "inspect selected thread TLS",
                    scenario.handle().main_global(global),
                )
                .await;
            let uscope::VariableValue::Scalar(ScalarValue::Signed(value)) =
                available_value(&variable.state)
            else {
                panic!("{fixture}: unavailable TLS variable {variable:?}");
            };
            values.push(*value);
            let pointer = scenario
                .operation(
                    "inspect selected thread TLS pointer",
                    scenario.handle().main_global(pointer),
                )
                .await;
            let reference = match pointer.state {
                VariableState::Available {
                    dereference: uscope::DereferenceState::Available(reference),
                    ..
                } => reference,
                state => panic!("{fixture}: unavailable TLS pointer {state:?}"),
            };
            thread_references.push((thread.id, *value, reference.clone()));
            let tls_referent = scenario
                .operation(
                    "dereference selected thread TLS pointer",
                    scenario.handle().dereference(reference),
                )
                .await;
            assert_dereferenced_scalar(&tls_referent, *value, fixture);
        }
        let selected = snapshot.threads.last().expect("TLS thread").id;
        scenario
            .operation(
                "change selection before reusing TLS capabilities",
                scenario.handle().select_thread(selected),
            )
            .await;
        for (origin, value, reference) in thread_references {
            assert_eq!(reference.thread(), origin);
            let tls_referent = scenario
                .operation(
                    "dereference TLS capability after changing selection",
                    scenario.handle().dereference(reference),
                )
                .await;
            assert_dereferenced_scalar(&tls_referent, value, fixture);
        }
        values.sort_unstable();
        assert_eq!(values, [300, 301, 302], "{fixture}");
        assert!(matches!(
            scenario.resume_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let after_exit = scenario.snapshot().await;
        assert_eq!(after_exit.threads.len(), 1, "{fixture}: {after_exit:?}");
        let surviving = scenario
            .operation(
                "inspect TLS after worker exit",
                scenario.handle().main_global(global),
            )
            .await;
        assert_variable_value(&surviving, ScalarValue::Signed(300));
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

#[tokio::test]
async fn static_locals_resolve_relocated_and_indexed_addresses() {
    for fixture in [
        "variables-static-gcc-o2",
        "variables-static-clang-o2",
        "variables-static-gcc-nopie",
    ] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
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
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
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
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
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
    let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
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

#[tokio::test]
async fn go_slices_decode_subranges_empty_and_nil_descriptors() {
    let fixture = "variables-go-o0";
    let mut scenario = Scenario::new("Go slice descriptors", Scenario::fixture(fixture));
    scenario.add_source_breakpoint("main.go", 88).await;
    run_go_to_breakpoint(&mut scenario, fixture).await;

    for (name, capacity, expected) in [
        ("values", Some(3), &[20_i128, 22][..]),
        ("empty", Some(4), &[][..]),
        ("nilSlice", Some(0), &[][..]),
    ] {
        let variable = scenario
            .operation(
                "inspect Go slice descriptor",
                scenario.handle().variable(name),
            )
            .await;
        assert_slice_values(&scenario, &variable, capacity, expected, fixture).await;
    }

    let mut reason = scenario.resume_to_stop().await;
    for _ in 0..32 {
        match reason {
            StopReason::Exited(ExitStatus::Code(0)) => break,
            StopReason::Breakpoint { .. } => reason = scenario.resume_to_stop().await,
            StopReason::Exception(ref exception) if exception.code == 23 => {
                reason = scenario.resume_to_stop().await;
            }
            _ => panic!("{fixture} stopped unexpectedly while exiting: {reason:?}"),
        }
    }
    assert_eq!(reason, StopReason::Exited(ExitStatus::Code(0)));
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
async fn optimized_go_debug_metadata_loads_without_advertising_runtime_control() {
    let debugger = Debugger::new(Scenario::fixture("variables-go-o2"))
        .expect("initialize optimized Go debugger");
    let handle = debugger.handle();
    let image = handle.module_image();
    let function = image
        .function_named("main.inspectScalars")
        .expect("optimized Go function metadata");
    assert!(
        image
            .instances_for_function(function.id)
            .any(|instance| matches!(instance.kind, CodeInstanceKind::OutOfLine))
    );
    assert!(
        image
            .source_files()
            .iter()
            .any(|source| source.path.ends_with("tests/fixtures/go/variables/main.go"))
    );
    drop(handle);
    debugger.shutdown().await.expect("shut down Go debugger");
}

#[tokio::test]
async fn zig_scalars_cover_pie_nonpie_and_optimized_partial_locations() {
    for fixture in ["variables-zig-o0", "variables-zig-nopie"] {
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
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
    let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
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
    assert!(
        snapshot
            .variables
            .iter()
            .any(|variable| { matches!(variable.state, VariableState::Unavailable(_)) })
    );
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
            StopReason::Exception(ref exception) if exception.code == 23 => {
                reason = scenario.resume_to_stop().await;
            }
            _ => panic!("{fixture} stopped unexpectedly while exiting: {reason:?}"),
        }
    }
    panic!("{fixture} did not exit after 32 runtime signals");
}

#[tokio::test]
async fn zig_o0_steps_through_inline_code_and_unwinds_logical_and_physical_frames() {
    let fixture = "stepping-boundaries-zig-o0";
    let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
    scenario
        .add_source_breakpoint("stepping-boundaries.zig", 29)
        .await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    assert_eq!(
        scenario.step_to_stop(StepKind::OverSource).await,
        StopReason::Step {
            kind: StepKind::OverSource
        }
    );
    assert_eq!(
        scenario.step_to_stop(StepKind::IntoSource).await,
        StopReason::Step {
            kind: StepKind::IntoSource
        }
    );
    let inline = scenario
        .operation("Zig inline location", scenario.handle().current_location())
        .await;
    assert_eq!(boundary_function(&inline), Some("inlineAdjust"));
    assert_eq!(boundary_line(&inline), Some(30));

    let trace = scenario
        .operation("Zig inline backtrace", scenario.handle().backtrace())
        .await;
    let names = trace
        .frames
        .iter()
        .filter_map(|frame| frame.function.as_ref())
        .map(|function| function.name.as_ref())
        .collect::<Vec<_>>();
    assert!(names.starts_with(&["inlineAdjust", "main"]), "{trace:?}");

    assert_eq!(
        scenario.step_to_stop(StepKind::Out).await,
        StopReason::Step {
            kind: StepKind::Out
        }
    );
    let caller = scenario
        .operation("Zig inline caller", scenario.handle().current_location())
        .await;
    assert_eq!(boundary_function(&caller), Some("main"));
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

#[tokio::test]
async fn optimized_zig_steps_into_and_finishes_a_physical_call() {
    let fixture = "stepping-boundaries-zig-o2";
    let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
    scenario
        .add_source_breakpoint("stepping-boundaries.zig", 29)
        .await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    assert_eq!(
        scenario.step_to_stop(StepKind::OverSource).await,
        StopReason::Step {
            kind: StepKind::OverSource
        }
    );
    assert_eq!(
        scenario.step_to_stop(StepKind::IntoSource).await,
        StopReason::Step {
            kind: StepKind::IntoSource
        }
    );
    let entered = scenario
        .operation("optimized Zig callee", scenario.handle().current_location())
        .await;
    assert_eq!(boundary_function(&entered), Some("markedReturns"));

    let trace = scenario
        .operation("optimized Zig backtrace", scenario.handle().backtrace())
        .await;
    let names = trace
        .frames
        .iter()
        .filter_map(|frame| frame.function.as_ref())
        .map(|function| function.name.as_ref())
        .collect::<Vec<_>>();
    assert!(names.starts_with(&["markedReturns", "main"]), "{trace:?}");

    assert_eq!(
        scenario.step_to_stop(StepKind::Out).await,
        StopReason::Step {
            kind: StepKind::Out
        }
    );
    let returned = scenario
        .operation("optimized Zig caller", scenario.handle().current_location())
        .await;
    assert_eq!(boundary_function(&returned), Some("main"));
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

#[tokio::test]
async fn zig_native_threads_are_all_stopped_selectable_and_variable_aware() {
    let fixture = "variables-threads-zig";
    let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
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
        match scenario.handle().variable("value").await {
            Ok(variable) => {
                let uscope::VariableValue::Scalar(ScalarValue::Unsigned(value)) =
                    available_value(&variable.state)
                else {
                    panic!("Zig worker value was not available: {variable:?}");
                };
                values.insert(*value);
            }
            Err(Error::LocationUnavailable | Error::VariableNotFound(_)) => {}
            Err(error) => panic!("unexpected Zig thread variable error: {error}"),
        }
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
async fn shutdown_reaps_a_stopped_zig_process_and_all_native_threads() {
    let fixture = "variables-threads-zig";
    let mut scenario = Scenario::new("shutdown Zig threads", Scenario::fixture(fixture));
    scenario
        .add_source_breakpoint("variables-threads.zig", 8)
        .await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    assert_eq!(scenario.snapshot().await.threads.len(), 3);

    let status = scenario.shutdown().await.expect("Zig inferior exit event");
    assert!(matches!(
        status,
        ExitStatus::Terminated(exception) if exception.code == 9
    ));
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
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
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
        let mut scenario = Scenario::new(fixture, Scenario::fixture(fixture));
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
async fn variable_inspection_refuses_an_ambiguous_inline_presentation() {
    let mut scenario = Scenario::new(
        "ambiguous inline stop",
        Scenario::fixture("variables-inline-gcc-o0"),
    );
    // A source breakpoint inside the inline body is attributed to both the
    // caller and the inline instance, so the stop has no single logical frame.
    scenario
        .add_source_breakpoint("variables-inline.c", 6)
        .await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    assert!(matches!(
        scenario.handle().variables().await,
        Err(Error::VariableContextUnsupported)
    ));
    scenario.shutdown().await;
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
                VariableState::Unavailable("no location at the current instruction".into()),
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
    assert!(context.file.path.ends_with("tests/fixtures/c/basic.c"));
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
        .add_file_function_breakpoint("tests/fixtures/c/basic.c", "breakpoint_target")
        .await;

    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let context = scenario
        .operation("source context", scenario.handle().source_context(0))
        .await;
    assert_eq!(context.location.line.get(), 6);
    scenario.shutdown().await;
}

#[tokio::test]
async fn breakpoint_deletion_preserves_shared_sites_and_stopped_instruction_execution() {
    let mut scenario = Scenario::new("breakpoint deletion", Scenario::fixture("basic"));
    let function = scenario.add_breakpoint("breakpoint_target").await;
    let source = scenario.add_source_breakpoint("basic.c", 6).await;
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

#[tokio::test]
async fn line_zero_rows_do_not_extend_the_previous_source_line() {
    let debugger =
        Debugger::new(Scenario::fixture("variables-rust-o0")).expect("initialize debugger");
    let image = debugger.handle().module_image().clone();
    let function = image
        .function_named("inspect_scalars")
        .expect("inspect_scalars definition");
    let instance = image
        .instances_for_function(function.id)
        .next()
        .expect("inspect_scalars instance");

    let mut attributed = 0_u64;
    let mut unattributed = 0_u64;
    for range in instance.ranges.iter() {
        for address in range.start.get()..range.end.get() {
            if image
                .locate(uscope::ImageAddress::new(address))
                .source
                .is_some()
            {
                attributed += 1;
            } else {
                unattributed += 1;
            }
        }
    }
    assert!(attributed > 0, "function body lost source attribution");
    assert!(
        unattributed > 0,
        "line-0 regions were attributed to a neighboring source line"
    );

    debugger.shutdown().await.expect("shutdown debugger");
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

    // The retained catalog describes the pre-exec program while the thread now
    // runs the replaced image; variable inspection must refuse rather than
    // resolve stale metadata against the new address space.
    assert!(matches!(
        scenario.handle().variables().await,
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
async fn source_steps_skip_non_statement_line_rows() {
    // GCC at -O2 marks the trailing rows of middle (line 13) and deepest
    // (line 8) as non-statement rows; source steps must not stop on them.
    // Clang does not emit new-line non-statement rows for this fixture, so
    // only the GCC binary exercises the defect.
    let mut next = Scenario::new("next unwind-o2", Scenario::fixture("unwind-o2"));
    next.add_breakpoint("middle").await;
    next.run_to_stop().await;

    assert_eq!(
        next.step_to_stop(StepKind::OverSource).await,
        StopReason::Step {
            kind: StepKind::OverSource
        }
    );
    let location = next
        .operation(
            "location after first next",
            next.handle().current_location(),
        )
        .await;
    assert_eq!(
        location
            .image
            .source
            .as_ref()
            .map(|source| source.line.get()),
        Some(12)
    );

    assert_eq!(
        next.step_to_stop(StepKind::OverSource).await,
        StopReason::Step {
            kind: StepKind::OverSource
        }
    );
    let location = next
        .operation(
            "location after second next",
            next.handle().current_location(),
        )
        .await;
    assert_eq!(
        location
            .image
            .function
            .as_ref()
            .map(|function| function.name.as_ref()),
        Some("outer"),
        "next stopped on a non-statement row instead of finishing middle"
    );
    next.shutdown().await;

    let mut step = Scenario::new("step unwind-o2", Scenario::fixture("unwind-o2"));
    step.add_breakpoint("deepest").await;
    step.run_to_stop().await;

    assert_eq!(
        step.step_to_stop(StepKind::IntoSource).await,
        StopReason::Step {
            kind: StepKind::IntoSource
        }
    );
    let location = step
        .operation(
            "location after first step",
            step.handle().current_location(),
        )
        .await;
    assert_eq!(
        location
            .image
            .source
            .as_ref()
            .map(|source| source.line.get()),
        Some(7)
    );

    assert_eq!(
        step.step_to_stop(StepKind::IntoSource).await,
        StopReason::Step {
            kind: StepKind::IntoSource
        }
    );
    let location = step
        .operation(
            "location after second step",
            step.handle().current_location(),
        )
        .await;
    assert_eq!(
        location
            .image
            .function
            .as_ref()
            .map(|function| function.name.as_ref()),
        Some("middle"),
        "step stopped on a non-statement row instead of returning to middle"
    );
    // The return address in middle sits on a non-statement row for the
    // already-executed call line (11); the step must continue to the next
    // statement row even though the activation changed at the return.
    assert_eq!(
        location
            .image
            .source
            .as_ref()
            .map(|source| source.line.get()),
        Some(12),
        "step completed on a non-statement row after the activation changed"
    );
    step.shutdown().await;
}

#[tokio::test]
async fn step_into_crosses_library_calls_without_line_info() {
    // The function breakpoint lands post-prologue on line 6, which calls
    // getpid() through the PLT. Its call-frame information uses a DWARF CFA
    // expression and its code has no line rows. A source step must cross the
    // library call and stop at line 7 instead of stopping inside the PLT or
    // failing the unwind.
    let mut scenario = Scenario::new("step over libc", Scenario::fixture("step-over-libc"));
    scenario.add_breakpoint("call_libc").await;
    scenario.run_to_stop().await;

    assert_eq!(
        scenario.step_to_stop(StepKind::IntoSource).await,
        StopReason::Step {
            kind: StepKind::IntoSource
        }
    );
    let location = scenario
        .operation(
            "location after library step",
            scenario.handle().current_location(),
        )
        .await;
    assert_eq!(
        location
            .image
            .function
            .as_ref()
            .map(|function| function.name.as_ref()),
        Some("call_libc")
    );
    assert_eq!(
        location
            .image
            .source
            .as_ref()
            .map(|source| source.line.get()),
        Some(7)
    );

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
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
    // The recommended post-prologue entry is the loop body itself, so leaving
    // the function breakpoint installed would intentionally interrupt the
    // finish plan on the next iteration instead of letting pause cancel it.
    scenario.remove_all_breakpoints().await;

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

    // Cancellation must also retract the plan's internal breakpoints: after
    // releasing the loop, a stale plan-owned site at the caller's return
    // address would surface as an unexpected breakpoint stop instead of exit.
    let release = scenario
        .operation(
            "resolve step release",
            scenario.handle().runtime_address("step_release"),
        )
        .await;
    scenario
        .operation(
            "release step loop",
            scenario.handle().write_word(release, 1),
        )
        .await;
    // The pause above was requested outside the scenario transcript loop, so
    // its stop event is still queued and must not satisfy the resume below.
    scenario.drain_pending_events();
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0)),
        "a canceled source-step plan left state that interrupted execution"
    );
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}
