//! Backtraces across modules and call-frame information.

use super::*;

#[tokio::test]
async fn dwarf_cfi_unwinds_and_finishes_across_the_compiler_and_linker_matrix() {
    for fixture in ["unwind-o0", "unwind-o2", "unwind-nopie", "unwind-clang-o2"] {
        let mut scenario = Scenario::launch(fixture);

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
        // Unwinding continues through libc's start code to its undefined
        // return address rather than stopping at the main image's boundary.
        assert_eq!(
            trace.termination,
            UnwindTermination::Complete,
            "unexpected {fixture} backtrace: {trace:?}"
        );

        // Finishing uses the same unwind information to find the caller.
        assert_eq!(
            scenario.step_to_stop(StepKind::Out).await,
            StopReason::Step {
                kind: StepKind::Out
            },
            "{fixture}"
        );
        let location = scenario
            .operation(
                "location after finish",
                scenario.handle().current_location(),
            )
            .await;
        assert_eq!(location_function(&location), Some("middle"), "{fixture}");

        scenario.shutdown().await;
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one stopped process visits each cross-module unwind boundary in order"
)]
async fn backtraces_unwind_through_shared_libraries_and_libc() {
    for fixture in [
        "module-frames-gcc-o0",
        "module-frames-clang-o2",
        "module-frames-gcc-nopie",
    ] {
        let mut scenario = Scenario::launch(fixture);
        let comparator = scenario.add_breakpoint("compare_values").await;
        scenario.add_breakpoint("module_callback").await;

        // main -> sort_values -> libc qsort -> compare_values: the comparator's
        // caller is described only by libc's own call-frame information.
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let modules = scenario
            .operation("modules", scenario.handle().loaded_modules())
            .await;
        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        let frames = frame_modules(&trace, &modules);
        assert_eq!(
            frames[0],
            (fixture.to_owned(), Some("compare_values".to_owned())),
            "{fixture}: {frames:#?}"
        );
        let sort = position_of(&frames, fixture, "sort_values");
        let main = position_of(&frames, fixture, "main");
        assert!(sort < main, "{fixture}: {frames:#?}");
        assert!(
            frames[1..sort]
                .iter()
                .all(|(module, _)| module.starts_with("libc.so")),
            "{fixture}: qsort frames must belong to libc: {frames:#?}"
        );
        assert!(
            sort > 1,
            "{fixture}: no libc frame was reconstructed: {frames:#?}"
        );
        // libc has no debug information, so its symbol tables name every
        // frame; so too the main image's start code, which has none either.
        for frame in trace.frames.iter() {
            assert!(
                frame.function.is_some() || frame.symbol.is_some(),
                "{fixture}: unnamed frame {frame:#?}"
            );
        }
        let start = trace.frames.last().expect("frames");
        assert_eq!(
            frames.last().map(|(module, _)| module.as_str()),
            Some(fixture)
        );
        assert_eq!(
            start.symbol.as_ref().map(|symbol| symbol.name.as_ref()),
            Some("_start"),
            "{fixture}: {start:#?}"
        );
        // glibc's entry code declares the return address undefined, so a
        // complete trace proves the walk crossed back through libc into _start.
        assert_eq!(
            trace.termination,
            UnwindTermination::Complete,
            "{fixture}: {frames:#?}"
        );
        assert!(
            frames[main + 1..]
                .iter()
                .any(|(module, _)| module.starts_with("libc.so")),
            "{fixture}: main's caller must be libc's start code: {frames:#?}"
        );

        // main -> dso_apply (shared library with DWARF) -> module_callback.
        scenario.remove_breakpoint(comparator.id).await;
        assert!(matches!(
            scenario.resume_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        let frames = frame_modules(&trace, &modules);
        assert_eq!(
            frames[..3],
            [
                (fixture.to_owned(), Some("module_callback".to_owned())),
                (
                    "libmodule-frames.so".to_owned(),
                    Some("dso_apply".to_owned())
                ),
                (fixture.to_owned(), Some("main".to_owned())),
            ],
            "{fixture}: {frames:#?}"
        );
        let library_source = trace.frames[1]
            .source
            .as_ref()
            .unwrap_or_else(|| panic!("{fixture}: dso_apply frame has no source"));
        assert!(
            trace.frames[1].code_instance.is_some(),
            "{fixture}: dso_apply frame lost its code instance"
        );
        assert_eq!(
            library_source.line.get(),
            6,
            "{fixture}: {library_source:?}"
        );
        assert_eq!(trace.termination, UnwindTermination::Complete);

        // abort() raises SIGABRT inside libc: the innermost frames have no
        // main-image metadata, yet the walk must still reach the caller.
        assert!(matches!(
            scenario.resume_to_stop().await,
            StopReason::Exception(exception) if exception.code == 6
        ));
        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        let frames = frame_modules(&trace, &modules);
        assert!(
            frames[0].0.starts_with("libc.so"),
            "{fixture}: SIGABRT must stop inside libc: {frames:#?}"
        );
        let caller = position_of(&frames, fixture, "abort_in_libc");
        assert!(
            caller < position_of(&frames, fixture, "main"),
            "{fixture}: {frames:#?}"
        );
        assert_eq!(
            trace.frames[caller - 1]
                .symbol
                .as_ref()
                .map(|symbol| symbol.name.as_ref()),
            Some("abort"),
            "{fixture}: {trace:#?}"
        );
        // The stop location is described by the module that contains it.
        let location = scenario
            .operation("location", scenario.handle().current_location())
            .await;
        let libc = modules
            .modules
            .iter()
            .find(|record| {
                record
                    .path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("libc.so"))
            })
            .expect("libc is loaded");
        assert_eq!(location.module, libc.module.id, "{fixture}");
        assert_eq!(location.image.symbol, trace.frames[0].symbol, "{fixture}");
        assert!(location.image.function.is_none(), "{fixture}");
        assert!(
            frames[..caller]
                .iter()
                .all(|(module, _)| module.starts_with("libc.so")),
            "{fixture}: {frames:#?}"
        );
        assert_eq!(trace.termination, UnwindTermination::Complete);

        scenario.shutdown().await;
    }
}

/// The kernel maps the vDSO from no file, so its module is read from the
/// process's memory. Its call-frame information carries the walk out of it
/// and restores the registers its callers keep values in, and its symbol
/// table names the code it exports; gdb names the same frames.
#[tokio::test]
async fn backtraces_unwind_through_the_vdso() {
    for variant in ["gcc-o0", "gcc-o2", "clang-o2-nopie"] {
        let fixture = format!("vdso-{variant}");
        // clock_gettime faults in an unexported helper that libc calls;
        // time is the vDSO's own, called straight from the program.
        for (mode, caller) in [("clock", "vdso_clock"), ("time", "vdso_time")] {
            let context = format!("{fixture} {mode}");
            let mut scenario = Scenario::launch(&fixture);
            let reason = scenario
                .run_with_to_stop(LaunchOptions {
                    arguments: vec![mode.into()],
                    ..LaunchOptions::default()
                })
                .await;
            assert!(
                matches!(&reason, StopReason::Exception(exception) if exception.code == 11),
                "{context}: {reason:?}"
            );
            let InferiorState::Stopped { process_id, .. } = scenario.snapshot().await.inferior
            else {
                panic!("{context}: the inferior is stopped");
            };
            let modules = scenario
                .operation("modules", scenario.handle().loaded_modules())
                .await;
            let vdso = support::vdso_module(&modules);
            // The kernel links its vDSO at zero.
            let mapping = support::vdso_mapping(process_id);
            assert_eq!(vdso.module.load_bias, mapping.start, "{context}");

            let trace = scenario
                .operation("backtrace", scenario.handle().backtrace())
                .await;
            let frames = frame_modules(&trace, &modules);
            assert!(
                mapping.contains(&trace.frames[0].instruction.get()),
                "{context}: {trace:#?}"
            );
            assert_eq!(frames[0].0, support::VDSO, "{context}: {frames:#?}");
            let level = position_of(&frames, &fixture, caller);
            let innermost = trace.frames[0].symbol.as_ref();
            if mode == "clock" {
                // No symbol covers the helper, and none is guessed.
                assert!(innermost.is_none(), "{context}: {innermost:?}");
                assert_eq!(level, 2, "{context}: {frames:#?}");
                assert!(frames[1].0.starts_with("libc.so"), "{context}: {frames:#?}");
                let wrapper = trace.frames[1].symbol.as_ref().expect("a libc symbol");
                assert!(
                    wrapper.name.ends_with("clock_gettime"),
                    "{context}: {wrapper:?}"
                );
            } else {
                // `time` and `__vdso_time` name the same code.
                let symbol = innermost.expect("a vDSO symbol");
                assert!(
                    ["time", "__vdso_time"].contains(&symbol.name.as_ref()) && symbol.offset > 0,
                    "{context}: {symbol:?}"
                );
                assert_eq!(level, 1, "{context}: {frames:#?}");
            }
            assert_eq!(
                frames[level + 1],
                (fixture.clone(), Some("main".to_owned())),
                "{context}: {frames:#?}"
            );
            assert_eq!(
                trace
                    .frames
                    .last()
                    .and_then(|frame| frame.symbol.as_ref())
                    .map(|symbol| symbol.name.as_ref()),
                Some("_start"),
                "{context}: {frames:#?}"
            );
            assert_eq!(
                trace.termination,
                UnwindTermination::Complete,
                "{context}: {frames:#?}"
            );

            // The stop is located in the vDSO, which has no source.
            let location = scenario
                .operation("location", scenario.handle().current_location())
                .await;
            assert_eq!(location.module, vdso.module.id, "{context}");
            assert_eq!(location.image.symbol, trace.frames[0].symbol, "{context}");
            assert!(location.image.source.is_none(), "{context}");

            // Optimized callers keep `depth` in a register the vDSO and libc
            // saved and restore through their call-frame information.
            scenario
                .operation(
                    "select caller",
                    scenario.handle().select_frame(trace.frames[level].id),
                )
                .await;
            let depth = scenario
                .operation("depth", scenario.handle().variable("depth"))
                .await;
            assert_signed(&depth.state, 42, &context);
            scenario.shutdown().await;
        }
    }
}
