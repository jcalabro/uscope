//! Debug metadata loaded without running the inferior.

use super::*;

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

    for fixture in ["types-cpp-gcc-dwarf4", "types-cpp-gcc-dwarf5"] {
        let image = load_fixture_image(fixture).await;
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
        let image = load_fixture_image(fixture).await;
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
