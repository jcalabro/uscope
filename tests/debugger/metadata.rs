//! Debug metadata loaded without running the inferior.

use super::*;

/// Clang emits the thunks a multiply inherited virtual destructor needs
/// with a linkage name and no other; the function is named by it.
#[tokio::test]
async fn functions_named_only_by_their_linkage_names_are_cataloged() {
    let image = load_fixture_image("containers-cpp-clang-o0").await;
    assert!(
        image
            .functions()
            .iter()
            .any(|function| function.name.contains("thunk")
                && function.name.contains("Tile::~Tile()")
                && function.linkage_name.as_deref() == Some("_ZThn16_N4TileD1Ev")),
        "{:?}",
        image
            .functions()
            .iter()
            .filter(|function| function.name.contains("thunk"))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn discarded_functions_are_not_cataloged_at_their_tombstone_addresses() {
    // LLD points the debug information of functions it discarded at address
    // 0. The Rust fixture has thousands; none may become code or line rows.
    let image = load_fixture_image("crash-rust-o0").await;
    let first_code = image
        .code_instances()
        .iter()
        .flat_map(|instance| instance.ranges.iter())
        .map(|range| range.start)
        .min()
        .expect("the fixture has code");
    assert!(first_code.get() > 0x1000, "code starts at {first_code}");
    assert!(
        image
            .statement_rows()
            .iter()
            .all(|row| row.address >= first_code),
        "a line row lies before the first function"
    );
    assert!(
        image
            .locate(uscope::ImageAddress::new(0))
            .function
            .is_none()
    );
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one pass over the compiler-language matrix checks its graphs and their semantics"
)]
async fn normalized_type_graphs_are_closed_and_preserve_language_semantics() {
    let mut images = BTreeMap::new();
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
        images.insert(fixture, image);
    }

    for fixture in ["variables-gcc-o0", "variables-clang-o0"] {
        let image = &images[fixture];
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
        let image = &images[fixture];
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

    for fixture in ["types-cpp-gcc-dwarf4", "types-cpp-gcc-dwarf5"] {
        let image = &images[fixture];
        for name in ["counted_global", "packed_global"] {
            let global = image
                .globals()
                .iter()
                .find(|global| global.name.as_ref() == name)
                .unwrap_or_else(|| panic!("{fixture}: missing global {name}"));
            let uscope::GlobalVariableType::Resolved(info) = &global.type_info else {
                panic!("{fixture}: {name} has type {:?}", global.type_info);
            };
            assert!(
                matches!(
                    &info.kind,
                    uscope::TypeKind::Record { members, .. } if members.len() == 1
                ),
                "{fixture}: {name} must be a one-member record: {info:?}"
            );
        }
    }

    for fixture in ["types-c-gcc-o0", "types-c-clang-o0"] {
        let image = &images[fixture];
        let global_type = |name: &str| {
            let global = image
                .globals()
                .iter()
                .find(|global| global.name.as_ref() == name)
                .unwrap_or_else(|| panic!("{fixture}: missing global {name}"));
            match &global.type_info {
                uscope::GlobalVariableType::Resolved(info) => info.clone(),
                other => panic!("{fixture}: {name} has type {other:?}"),
            }
        };
        for (name, count) in [("byte_bounded", 256), ("short_bounded", 65536)] {
            assert!(
                matches!(
                    &global_type(name).kind,
                    uscope::TypeKind::Array { dimensions, .. } if dimensions[0].count == count
                ),
                "{fixture}: {name}"
            );
        }
        for (name, expected) in [
            ("const_void_pointer", "const void *"),
            ("typedef_void_pointer", "opaque_handle *"),
        ] {
            assert_eq!(
                global_type(name).name.as_ref(),
                expected,
                "{fixture}: {name}"
            );
        }
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

    // Every function carries the language of the unit defining it.
    for (fixture, function, language) in [
        ("variables-gcc-o0", "main", uscope::SourceLanguage::C),
        (
            "variables-cpp-clang-o0",
            "main",
            uscope::SourceLanguage::Cpp,
        ),
        ("variables-rust-o0", "main", uscope::SourceLanguage::Rust),
        (
            "variables-zig-o0",
            "inspectScalars",
            uscope::SourceLanguage::Zig,
        ),
        (
            "variables-go-o0",
            "inspectScalars",
            uscope::SourceLanguage::Go,
        ),
    ] {
        let found = images[fixture]
            .functions()
            .iter()
            .find(|info| info.name.rsplit(['.', ':']).next() == Some(function))
            .unwrap_or_else(|| panic!("{fixture}: no function {function}"));
        assert_eq!(found.language, language, "{fixture}: {found:?}");
    }

    let go = &images["variables-go-o0"];
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

    let zig = &images["variables-zig-o0"];
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

    let rust = &images["variables-rust-o0"];
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

#[tokio::test]
async fn dwarf_normalization_preserves_inline_instances_and_line_rows() {
    for fixture in [
        "inline-gcc-o1",
        "inline-gcc-o2",
        "inline-clang-o1",
        "inline-clang-o2",
    ] {
        assert_inline_metadata(&*load_fixture_image(fixture).await, fixture);
    }
}

#[tokio::test]
async fn line_zero_rows_do_not_extend_the_previous_source_line() {
    let image = load_fixture_image("variables-rust-o0").await;
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
}

/// A line table's last row for a function runs on, to the next row,
/// through code the compiler did not describe, such as hand-written
/// assembly placed after it. That code has no source line.
#[tokio::test]
async fn hand_written_assembly_after_a_function_has_no_source_line() {
    let image = load_fixture_image("assembly-after-code").await;
    let bare = image
        .symbols()
        .iter()
        .find(|symbol| symbol.name.as_ref() == "bare")
        .expect("the fixture defines bare");
    let extent = bare.extent.expect("bare has a size").range;
    // The fixture's layout: no row begins in bare, and the row before it
    // runs on past it.
    let rows = image.statement_rows();
    assert!(
        rows.iter().all(|row| !extent.contains(row.address)),
        "a row describes bare"
    );
    assert!(
        rows.iter()
            .any(|row| row.address < extent.start && row.location.is_some())
            && rows
                .iter()
                .filter(|row| row.address >= extent.start)
                .all(|row| row.address >= extent.end),
        "bare does not follow a described function"
    );
    let located = image.locate(extent.start);
    assert_eq!(located.source, None, "{located:?}");
    assert!(located.function.is_none());
    // The row before still describes the function it belongs to.
    let before = uscope::ImageAddress::new(extent.start.get() - 1);
    assert!(image.locate(before).source.is_some());
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

/// Go code's roles follow the runtime's own traceback, from the function
/// table's flags and IDs and from names, with DWARF and without it. An ABI
/// wrapper shares its function's name everywhere but has a role of its own.
#[tokio::test]
async fn go_code_roles_follow_the_runtimes_traceback() {
    use uscope::CodeRole::{
        Ordinary, Outermost, RuntimeInternal, SignalTrampoline, StackSwitch, TrapEntry, Wrapper,
    };
    for fixture in ["callers-go", "callers-go-stripped"] {
        let image = load_fixture_image(fixture).await;
        let roles = |name: &str| {
            let mut roles = image
                .functions_named(name)
                .flat_map(|function| image.instances_for_function(function.id))
                .filter(|instance| matches!(instance.kind, CodeInstanceKind::OutOfLine))
                .map(|instance| {
                    let role = image.code_role(instance.ranges[0].start);
                    let function = image.function(instance.function).expect("a function");
                    assert_eq!(function.role, role, "{fixture}: {name}");
                    format!("{role:?}")
                })
                .collect::<Vec<_>>();
            roles.sort();
            roles
        };
        for (name, expected) in [
            ("runtime.newproc", &[RuntimeInternal, Wrapper][..]),
            ("runtime.asmcgocall", &[StackSwitch, Wrapper]),
            ("runtime.goexit", &[Outermost]),
            ("runtime.mstart", &[Outermost]),
            ("runtime.rt0_go", &[Outermost]),
            ("runtime.systemstack", &[StackSwitch]),
            ("runtime.mcall", &[StackSwitch]),
            ("runtime.morestack", &[StackSwitch]),
            ("runtime.sigpanic", &[TrapEntry]),
            ("runtime.asyncPreempt", &[TrapEntry]),
            ("runtime.sigreturn__sigaction", &[SignalTrampoline]),
            // The kernel enters the signal handler with the signal
            // trampoline as its return address, so it is not outermost.
            ("runtime.sigtramp", &[RuntimeInternal]),
            ("runtime.deferreturn", &[Wrapper]),
            ("runtime.mallocgc", &[RuntimeInternal]),
            ("runtime.(*mheap).alloc", &[RuntimeInternal]),
            (
                "internal/runtime/syscall/linux.Syscall6",
                &[RuntimeInternal],
            ),
            ("gogo", &[StackSwitch]),
            ("runtime.SetFinalizer", &[Ordinary]),
            // A closure of an exported function is not exported.
            ("runtime.SetFinalizer.func2", &[RuntimeInternal]),
            ("runtime.(*Frames).Next", &[Ordinary]),
            ("main.main", &[Ordinary]),
            ("main.(*walker).descend", &[Ordinary]),
        ] {
            let mut expected = expected
                .iter()
                .map(|role| format!("{role:?}"))
                .collect::<Vec<_>>();
            expected.sort();
            assert_eq!(roles(name), expected, "{fixture}: {name}");
        }
        // Symbols carry the same roles, the ABI0 one by its suffixed name.
        if fixture == "callers-go" {
            for (name, role) in [
                ("runtime.newproc", RuntimeInternal),
                ("runtime.newproc.abi0", Wrapper),
                ("runtime.systemstack.abi0", StackSwitch),
                ("runtime.goexit.abi0", Outermost),
            ] {
                let symbol = image.symbol_named(name).expect("a symbol");
                assert_eq!(symbol.role, role, "{name}");
            }
        }
    }
}
