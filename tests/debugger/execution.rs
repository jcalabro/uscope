//! Breakpoints, threads, signals, memory, and session lifecycle.

use super::*;

#[tokio::test]
async fn raw_memory_reads_publish_prefixes_at_unmapped_boundaries() {
    for fixture in ["pointer-memory-gcc-o0", "pointer-memory-clang-o0"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("inspect_boundaries").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let boundary_array = scenario
            .operation(
                "inspect boundary array pointer",
                scenario.handle().variable("boundary_array"),
            )
            .await;
        let boundary_address = match available_value(&boundary_array.state) {
            uscope::VariableValue::Address(value) => value.address,
            value => panic!("{fixture}: boundary array was not an address: {value:?}"),
        };
        let readable_prefix = scenario
            .operation(
                "read memory across an unmapped boundary",
                scenario.handle().read_memory(boundary_address, 16),
            )
            .await;
        assert_eq!(
            readable_prefix.bytes.as_ref(),
            [41_i32.to_le_bytes(), 42_i32.to_le_bytes()].concat()
        );
        assert_eq!(
            readable_prefix.completion,
            uscope::MemoryReadCompletion::Incomplete {
                next_address: VirtualAddress::new(boundary_address.get() + 8),
                reason: uscope::MemoryReadUnavailableReason::Inaccessible,
            }
        );

        let inaccessible_address = VirtualAddress::new(boundary_address.get() + 8);
        let inaccessible = scenario
            .operation(
                "read wholly inaccessible memory",
                scenario.handle().read_memory(inaccessible_address, 8),
            )
            .await;
        assert!(inaccessible.bytes.is_empty(), "{inaccessible:?}");
        assert_eq!(
            inaccessible.completion,
            uscope::MemoryReadCompletion::Incomplete {
                next_address: inaccessible_address,
                reason: uscope::MemoryReadUnavailableReason::Inaccessible,
            }
        );

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        scenario.shutdown().await;
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
async fn raw_memory_reads_are_bounded_stop_scoped_and_hide_breakpoints() {
    let mut scenario = Scenario::new("raw memory", Scenario::fixture("basic"));
    scenario.add_breakpoint("breakpoint_target").await;
    let first_address = match scenario.run_to_stop().await {
        StopReason::Breakpoint { address } => address,
        other => panic!("expected breakpoint, got {other:?}"),
    };
    let value_address = scenario
        .operation(
            "resolve uscope_value",
            scenario.handle().runtime_address("uscope_value"),
        )
        .await;
    let stopped = scenario.snapshot().await;

    let value_bytes = scenario
        .operation(
            "read uscope_value bytes",
            scenario.handle().read_memory(value_address, 8),
        )
        .await;
    assert_eq!(
        value_bytes.bytes.as_ref(),
        0x1122_3344_5566_7788_u64.to_le_bytes()
    );
    assert_eq!(
        value_bytes.completion,
        uscope::MemoryReadCompletion::Complete
    );
    assert_eq!(value_bytes.revision, scenario.last_revision());
    assert_eq!(Some(value_bytes.stop_id), stopped.stop_id);
    assert_eq!(
        value_bytes.target,
        scenario.handle().module_image().target()
    );

    let breakpoint_bytes = scenario
        .operation(
            "read logical breakpoint bytes",
            scenario.handle().read_memory(first_address, 1),
        )
        .await;
    assert_eq!(
        breakpoint_bytes.completion,
        uscope::MemoryReadCompletion::Complete
    );
    assert_ne!(
        breakpoint_bytes.bytes.as_ref(),
        [0xcc],
        "the installed trap leaked through the logical memory API"
    );

    let empty = scenario
        .operation(
            "read an empty memory range",
            scenario.handle().read_memory(value_address, 0),
        )
        .await;
    assert!(empty.bytes.is_empty());
    assert_eq!(empty.completion, uscope::MemoryReadCompletion::Complete);
    assert!(matches!(
        scenario.handle().read_memory(value_address, 65_537).await,
        Err(Error::MemoryReadTooLarge {
            requested: 65_537,
            maximum: 65_536,
        })
    ));
    assert!(matches!(
        scenario
            .handle()
            .read_memory(VirtualAddress::new(u64::MAX), 1)
            .await,
        Err(Error::AddressOverflow)
    ));

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

#[tokio::test]
async fn stops_in_a_shared_library_show_that_librarys_own_source() {
    let mut scenario = Scenario::new("library stop", Scenario::fixture("module-frames-gcc-o0"));
    scenario.add_breakpoint("main").await;
    scenario.run_to_stop().await;
    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    let library = modules
        .modules
        .iter()
        .find(|record| record.path.ends_with("libmodule-frames.so"))
        .expect("library is loaded");
    let image = scenario
        .operation(
            "library image",
            scenario.handle().loaded_module_image(library.module.id),
        )
        .await;
    let function = image.function_named("dso_apply").expect("dso_apply");
    let entry = image
        .instances_for_function(function.id)
        .find_map(|instance| instance.breakpoint_entry)
        .expect("dso_apply entry");
    let address = library
        .module
        .virtual_address(entry.address)
        .expect("relocated entry");
    scenario
        .add_breakpoint_spec(BreakpointSpec::Address(address))
        .await;
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    // Source-file identifiers belong to the library's image; resolving them
    // in the main image would name main.c, or no file at all.
    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;
    assert_eq!(location.module, library.module.id);
    assert_eq!(
        location.image.function.map(|function| function.name),
        Some(Arc::from("dso_apply"))
    );
    let context = scenario
        .operation("source", scenario.handle().source_context(1))
        .await;
    assert!(
        context
            .file
            .path
            .ends_with("tests/fixtures/c/module-frames/library.c"),
        "{context:#?}"
    );
    assert!(
        context
            .lines
            .iter()
            .any(|line| line.text.contains("callback(adjusted)")),
        "{context:#?}"
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn inline_function_breakpoints_resolve_every_concrete_instance() {
    for fixture in [
        "inline-gcc-o1",
        "inline-gcc-o2",
        "inline-clang-o1",
        "inline-clang-o2",
    ] {
        let mut scenario = Scenario::launch(fixture);
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
        let mut scenario = Scenario::launch(fixture);
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
    let (process_id, stop_id, selected) = match snapshot.inferior {
        InferiorState::Stopped {
            process_id,
            stop_id,
            thread_id,
            ..
        } => (process_id, stop_id, thread_id),
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

    // Unknown and unrepresentable thread IDs fail without disturbing the stop.
    for unknown in [0, 1 << 40, u64::MAX].map(uscope::ThreadId::new) {
        assert!(matches!(
            scenario.handle().select_thread(unknown).await,
            Err(Error::UnknownThread(thread)) if thread == unknown
        ));
        assert!(matches!(
            scenario
                .handle()
                .continue_execution(
                    stop_id,
                    uscope::ResumeScope::Thread(unknown),
                    uscope::ExceptionDisposition::Pass,
                )
                .await,
            Err(Error::UnknownThread(thread)) if thread == unknown
        ));
    }
    assert_eq!(scenario.snapshot().await.stop_id, Some(stop_id));

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
async fn forked_children_are_released_without_inherited_breakpoints() {
    let mut scenario = Scenario::launch("fork");
    scenario.add_breakpoint("shared_work").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    // The child also runs shared_work; an inherited trap would kill it with
    // SIGTRAP. The parent's SIGCHLD reports a normal exit (CLD_EXITED).
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exception(exception)
            if exception.code == 17 && exception.description.contains("si_code 1")
    ));
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn stops_survive_executable_mappings_that_name_no_file() {
    let mut scenario = Scenario::new("memfd mapping", Scenario::fixture("memfd-exec"));
    scenario.add_breakpoint("after_mapping").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
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
    // runs the replaced image; every view through it must refuse rather than
    // resolve stale metadata against the new address space.
    assert!(matches!(
        scenario.handle().variables().await,
        Err(Error::Backend(_))
    ));
    assert!(matches!(
        scenario.handle().backtrace().await,
        Err(Error::Backend(_))
    ));
    assert!(matches!(
        scenario.handle().current_location().await,
        Err(Error::Backend(_))
    ));
    assert!(matches!(
        scenario.snapshot().await.presentation,
        Some(uscope::FramePresentation {
            frame: uscope::PresentedFrame::Physical,
            ..
        })
    ));

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
async fn pause_during_launch_ends_the_launch_execution_in_a_coherent_stop() {
    let mut scenario = Scenario::new("pause during launch", Scenario::fixture("spin"));
    let run = scenario.start_launching().await;

    // The initial exec stop may be processed before or after this request;
    // either order must end the launch execution in one inspectable pause.
    let reason = timeout(Duration::from_secs(2), scenario.handle().pause())
        .await
        .expect("pause timed out")
        .expect("pause failed");
    assert_eq!(reason, StopReason::Pause);
    let launched = timeout(Duration::from_secs(2), run)
        .await
        .expect("run timed out")
        .expect("run task panicked")
        .expect("run failed");
    assert_eq!(launched, StopReason::Pause);
    assert!(matches!(
        scenario.snapshot().await.inferior,
        InferiorState::Stopped {
            reason: StopReason::Pause,
            ..
        }
    ));
    scenario
        .operation("read paused registers", scenario.handle().registers())
        .await;

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
                uscope::StackFrameId::INNERMOST,
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

#[tokio::test]
async fn source_path_maps_find_sources_of_programs_built_elsewhere() {
    use std::path::{Path, PathBuf};
    use support::ScratchDir;
    use uscope::{Error, SourcePathMap};

    let mut scenario = Scenario::launch("basic-relocated");
    scenario.add_source_breakpoint("basic.c", 11).await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let recorded = PathBuf::from("/nonexistent/uscope/tests/fixtures/c/basic.c");
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
    let map = |rules: &[(&Path, &Path)]| {
        let mut map = SourcePathMap::new();
        for (from, to) in rules {
            map.push(from, to).unwrap();
        }
        scenario.handle().clone().with_source_paths(map)
    };

    // Unmapped, the recorded path names nothing on this machine.
    assert!(matches!(
        scenario.handle().source_context(0).await,
        Err(Error::SourceFileMissing { path, tried }) if path == recorded && tried == [recorded.clone()]
    ));

    // The first rewrite that exists is read and reported; the debug
    // information keeps naming the recorded file.
    let copy = ScratchDir::new("source-map");
    let copied = copy.path().join("tests/fixtures/c/basic.c");
    fs::create_dir_all(copied.parent().unwrap()).unwrap();
    let source = fs::read_to_string(repository.join("tests/fixtures/c/basic.c")).unwrap();
    fs::write(
        &copied,
        source.replace(
            "first = breakpoint_target();",
            "first = breakpoint_target(); /* copy */",
        ),
    )
    .unwrap();
    let absent = copy.path().join("absent");
    let handle = map(&[
        (
            Path::new("/nonexistent/uscope/tests/fixtures/c/basic.c/x"),
            &absent,
        ),
        (Path::new("/nonexistent/uscope"), &absent),
        (
            Path::new("/nonexistent/uscope/tests"),
            &copy.path().join("tests"),
        ),
        (Path::new("/nonexistent"), repository.parent().unwrap()),
    ]);
    let context = scenario
        .operation("mapped source", handle.source_context(1))
        .await;
    assert_eq!(*context.path, copied);
    assert_eq!(*context.file.path, recorded);
    assert_eq!(context.location.line.get(), 11);
    assert_eq!(
        &*context.lines[0].text,
        "    uint64_t first = breakpoint_target(); /* copy */"
    );

    // Rules match whole leading components, and a missing source names
    // every place it was looked for.
    let handle = map(&[
        (Path::new("/nonexist"), repository),
        (Path::new("/nonexistent/uscope"), &absent),
    ]);
    assert!(matches!(
        handle.source_context(0).await,
        Err(Error::SourceFileMissing { path, tried })
            if path == recorded && tried == [absent.join("tests/fixtures/c/basic.c"), recorded.clone()]
    ));

    // A candidate that exists but cannot be read is reported, not skipped.
    let unreadable = copy.path().join("unreadable");
    fs::create_dir_all(unreadable.join("tests/fixtures/c/basic.c")).unwrap();
    let handle = map(&[
        (Path::new("/nonexistent/uscope"), &unreadable),
        (Path::new("/nonexistent/uscope"), repository),
    ]);
    assert!(matches!(
        handle.source_context(0).await,
        Err(Error::SourceFileRead { path, .. }) if path == unreadable.join("tests/fixtures/c/basic.c")
    ));

    // Each handle keeps its own map.
    let mapped = map(&[(Path::new("/nonexistent/uscope"), repository)]);
    assert_eq!(
        *scenario
            .operation("repository source", mapped.source_context(0))
            .await
            .path,
        repository.join("tests/fixtures/c/basic.c")
    );
    assert!(scenario.handle().source_context(0).await.is_err());
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(uscope::ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}
