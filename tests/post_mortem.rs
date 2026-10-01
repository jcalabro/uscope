mod support;

use std::fs;
use std::path::{Path, PathBuf};

use object::read::elf::{FileHeader as _, ProgramHeader as _};
use object::{Endianness, elf};
use uscope::{
    Backtrace, CoreDumpInfo, CoreDumpOptions, CoreModuleState, Debugger, Error,
    ExceptionDisposition, InferiorState, LoadedModuleSnapshot, MemoryReadCompletion,
    ModuleIdentity, ProcessId, RegisterRole, ResumeScope, ScalarValue, StepKind, StopId,
    StopReason, ThreadState, UnwindTermination, Variable, VariableState, VariableUnavailableReason,
    VariableValue, VirtualAddress,
};

use support::{Scenario, ScratchDir};

const MATRIX: [&str; 3] = ["gcc-o0", "clang-o2", "gcc-o2-nopie"];
const WORKERS: u64 = 3;

fn core(name: &str) -> PathBuf {
    Scenario::fixture(name)
}

fn open_core(name: &str) -> Scenario {
    Scenario::open_core(name, &CoreDumpOptions::new(core(name)))
}

fn options(core_name: &str, executable: Option<&str>, allow: bool) -> CoreDumpOptions {
    CoreDumpOptions {
        core: core(core_name),
        executable: executable.map(Scenario::fixture),
        allow_module_mismatch: allow,
    }
}

/// A fresh directory for one test's modified copies of read-only fixtures.
fn crash_source_line(needle: &str) -> u64 {
    let source = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/c/crash/main.c"),
    )
    .expect("read crash fixture source");
    let index = source
        .lines()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("crash fixture has no line containing {needle:?}"));
    u64::try_from(index + 1).expect("line fits u64")
}

fn available(variable: &Variable) -> &VariableValue {
    match &variable.state {
        VariableState::Available { value, .. } => value,
        state => panic!("{} was not available: {state:?}", variable.name),
    }
}

fn signed(variable: &Variable) -> i128 {
    match available(variable) {
        VariableValue::Scalar(ScalarValue::Signed(value)) => *value,
        value => panic!("{} was not a signed scalar: {value:?}", variable.name),
    }
}

fn binary64(variable: &Variable) -> f64 {
    match available(variable) {
        VariableValue::Scalar(ScalarValue::Floating(uscope::FloatValue::Binary64(bits))) => {
            f64::from_bits(*bits)
        }
        value => panic!("{} was not a binary64 scalar: {value:?}", variable.name),
    }
}

async fn variable(scenario: &Scenario, name: &str) -> Variable {
    scenario
        .operation(name, scenario.handle().variable(name))
        .await
}

fn register(registers: &uscope::RegisterSnapshot, role: RegisterRole) -> u64 {
    let value = registers
        .registers
        .iter()
        .find(|value| value.register.role == Some(role))
        .unwrap_or_else(|| panic!("missing {role:?} register"));
    u64::from_le_bytes(value.bytes.as_ref().try_into().expect("64-bit register"))
}

/// Names each frame by its owning module's file name and its function.
fn frames(trace: &Backtrace, modules: &LoadedModuleSnapshot) -> Vec<(String, Option<String>)> {
    trace
        .frames
        .iter()
        .map(|frame| {
            let module = frame.module.map_or_else(
                || "?".to_owned(),
                |id| {
                    modules
                        .modules
                        .iter()
                        .find(|record| record.module.id == id)
                        .and_then(|record| record.path.file_name())
                        .map_or_else(
                            || panic!("frame names unknown module {id:?}"),
                            |name| name.to_string_lossy().into_owned(),
                        )
                },
            );
            let function = frame
                .function
                .as_ref()
                .map(|function| function.name.to_string());
            (module, function)
        })
        .collect()
}

async fn named_frames(scenario: &Scenario) -> (Backtrace, Vec<(String, Option<String>)>) {
    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let names = frames(&trace, &modules);
    (trace, names)
}

fn position(frames: &[(String, Option<String>)], module: &str, function: &str) -> usize {
    frames
        .iter()
        .position(|(frame_module, name)| {
            frame_module == module && name.as_deref() == Some(function)
        })
        .unwrap_or_else(|| panic!("no {module}:{function} frame in {frames:#?}"))
}

fn core_info(scenario: &Scenario) -> CoreDumpInfo {
    scenario
        .handle()
        .core_dump()
        .expect("a core session describes its dump")
        .as_ref()
        .clone()
}

fn loaded_identity<'a>(info: &'a CoreDumpInfo, file_name: &str) -> &'a ModuleIdentity {
    info.modules
        .iter()
        .find_map(|module| match &module.state {
            CoreModuleState::Loaded { module, identity }
                if module
                    .path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(file_name)) =>
            {
                Some(identity)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("{file_name} was not loaded: {info:#?}"))
}

struct StoppedCore {
    process: ProcessId,
    stop: StopId,
    thread: uscope::ThreadId,
    reason: StopReason,
}

async fn stopped(scenario: &mut Scenario) -> (StoppedCore, uscope::StateSnapshot) {
    let snapshot = scenario.snapshot().await;
    let InferiorState::Stopped {
        process_id,
        stop_id,
        thread_id,
        reason,
    } = snapshot.inferior.clone()
    else {
        panic!("a core is always stopped: {snapshot:?}");
    };
    assert_eq!(snapshot.stop_id, Some(stop_id));
    (
        StoppedCore {
            process: process_id,
            stop: stop_id,
            thread: thread_id,
            reason,
        },
        snapshot,
    )
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one opened core is checked through every inspection boundary in order"
)]
async fn segv_cores_present_the_faulting_frame_across_the_compiler_matrix() {
    let fault_line = crash_source_line("*record->poison = depth;");
    for variant in MATRIX {
        let fixture = format!("crash-{variant}");
        let mut scenario = open_core(&format!("{fixture}-segv.core"));

        let (stop, snapshot) = stopped(&mut scenario).await;
        let StopReason::CoreDump {
            exception: Some(exception),
        } = &stop.reason
        else {
            panic!("{variant}: unexpected stop reason {:?}", stop.reason);
        };
        assert_eq!(exception.code, 11, "{variant}");
        assert!(
            exception
                .description
                .contains("SIGSEGV (SEGV_MAPERR) at 0x0"),
            "{variant}: {exception:?}"
        );
        assert_eq!(snapshot.selected_thread, Some(stop.thread));
        assert_eq!(
            snapshot.threads.len(),
            1 + usize::try_from(WORKERS).unwrap()
        );
        assert!(snapshot.threads.iter().all(|thread| {
            matches!(
                &thread.state,
                ThreadState::Stopped { reason } if (thread.id == stop.thread) == reason.is_some()
            )
        }));

        let info = core_info(&scenario);
        assert_eq!(info.process_id, stop.process);
        assert_eq!(info.exception.as_ref(), Some(exception));
        // pr_fname keeps a bounded prefix of the executable name; gdb fills
        // all 16 bytes while the kernel reserves one for a terminator.
        assert!(
            fixture.starts_with(info.process_name.as_ref())
                && info.process_name.len() >= fixture.len().min(15),
            "{variant}: {:?}",
            info.process_name
        );
        // pr_psargs keeps a bounded prefix of the command line.
        let command_line = format!("{} segv", Scenario::fixture(&fixture).display());
        assert!(
            !info.arguments.is_empty() && command_line.starts_with(info.arguments.as_ref()),
            "{info:?}"
        );
        let CoreModuleState::Loaded { module, .. } = &info.modules[0].state else {
            panic!("{variant}: the executable was not loaded: {info:?}");
        };
        assert_eq!(
            module.path.as_ref(),
            &Scenario::fixture(&fixture).canonicalize().unwrap()
        );
        // Build-ids are only linked into the gcc -O0 variant; the others are
        // proven by their saved read-only bytes. Every library has a build-id.
        let executable_identity = loaded_identity(&info, &fixture);
        if variant == "gcc-o0" {
            assert_eq!(executable_identity, &ModuleIdentity::BuildId);
        } else {
            assert!(
                matches!(executable_identity, ModuleIdentity::SavedContent { compared_bytes } if *compared_bytes > 0),
                "{variant}: {executable_identity:?}"
            );
        }
        for library in ["libc.so", "libcrash.so", "ld-linux"] {
            assert_eq!(
                loaded_identity(&info, library),
                &ModuleIdentity::BuildId,
                "{variant}: {library}"
            );
        }

        let location = scenario
            .operation("location", scenario.handle().current_location())
            .await;
        assert_eq!(
            location
                .image
                .function
                .as_ref()
                .map(|function| function.name.as_ref()),
            Some("crash_segv"),
            "{variant}"
        );
        assert_eq!(
            location
                .image
                .source
                .as_ref()
                .map(|source| source.line.get()),
            Some(fault_line),
            "{variant}"
        );
        let registers = scenario
            .operation("registers", scenario.handle().registers())
            .await;
        assert_eq!(
            register(&registers, RegisterRole::ProgramCounter),
            location.address.get()
        );

        let (trace, names) = named_frames(&scenario).await;
        assert_eq!(names[0], (fixture.clone(), Some("crash_segv".to_owned())));
        assert_eq!(names[1], (fixture.clone(), Some("main".to_owned())));
        assert!(names[2].0.starts_with("libc.so"), "{variant}: {names:#?}");
        assert_eq!(trace.termination, UnwindTermination::Complete, "{variant}");

        assert_eq!(signed(&variable(&scenario, "depth").await), 3, "{variant}");
        // At -O2 `scale` lives in xmm0, read from the dump's NT_FPREGSET.
        assert!(
            (binary64(&variable(&scenario, "scale").await) - 2.5).abs() < f64::EPSILON,
            "{variant}"
        );
        assert_eq!(signed(&variable(&scenario, "crash_counter").await), 8);
        assert_eq!(
            signed(&variable(&scenario, "crash_library_value").await),
            321
        );
        assert_eq!(signed(&variable(&scenario, "crash_tls").await), 100);
        assert_eq!(signed(&variable(&scenario, "crash_library_tls").await), 654);
        let record = scenario
            .operation(
                "record id",
                scenario.handle().inspect(
                    uscope::parse_value_expression("(*record).id")
                        .unwrap()
                        .expression,
                ),
            )
            .await;
        if variant == "gcc-o2-nopie" {
            // GCC describes the dead pointer only by its caller-side entry
            // value, which is reported rather than guessed.
            assert!(
                matches!(
                    &record.state,
                    VariableState::Unavailable(VariableUnavailableReason::Unsupported(
                        uscope::UnsupportedVariableFeature::ParameterReference
                    ))
                ),
                "{variant}: {record:?}"
            );
        } else {
            assert!(
                matches!(
                    &record.state,
                    VariableState::Available {
                        value: VariableValue::Scalar(ScalarValue::Signed(42)),
                        ..
                    }
                ),
                "{variant}: {record:?}"
            );
        }

        // .rodata is never saved by the dump; a verified file supplies it.
        let message = scenario
            .operation(
                "crash_message address",
                scenario.handle().runtime_address("crash_message"),
            )
            .await;
        let read = scenario
            .operation("read message", scenario.handle().read_memory(message, 29))
            .await;
        assert_eq!(read.completion, MemoryReadCompletion::Complete);
        assert_eq!(read.bytes.as_ref(), b"post-mortem read-only message");
        assert_eq!(read.stop_id, stop.stop);
        let unmapped = scenario
            .operation(
                "read null",
                scenario.handle().read_memory(VirtualAddress::new(0), 8),
            )
            .await;
        assert!(unmapped.bytes.is_empty());
        assert!(matches!(
            unmapped.completion,
            MemoryReadCompletion::Incomplete { next_address, .. } if next_address == VirtualAddress::new(0)
        ));

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn abort_cores_unwind_from_libc_into_the_aborting_caller() {
    for variant in MATRIX {
        let fixture = format!("crash-{variant}");
        let mut scenario = open_core(&format!("{fixture}-abort.core"));
        let (stop, _) = stopped(&mut scenario).await;
        let StopReason::CoreDump {
            exception: Some(exception),
        } = &stop.reason
        else {
            panic!("{variant}: unexpected stop reason {:?}", stop.reason);
        };
        assert_eq!(exception.code, 6);
        // abort() signals its own thread, so the sender is the process itself.
        assert!(
            exception.description.contains(&format!(
                "SIGABRT (SI_TKILL) sent by process {}",
                stop.process
            )),
            "{variant}: {exception:?}"
        );

        let (trace, names) = named_frames(&scenario).await;
        let caller = position(&names, &fixture, "crash_abort");
        assert!(caller > 0, "{variant}: {names:#?}");
        assert!(
            names[..caller]
                .iter()
                .all(|(module, _)| module.starts_with("libc.so")),
            "{variant}: abort's frames must belong to libc: {names:#?}"
        );
        assert_eq!(
            names[caller + 1],
            (fixture.clone(), Some("main".to_owned()))
        );
        assert_eq!(trace.termination, UnwindTermination::Complete);

        // The innermost frame is libc's: no main-image locals are in scope.
        assert!(matches!(
            scenario.handle().variables().await,
            Err(Error::VariableContextUnsupported)
        ));
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn every_dumped_thread_keeps_its_own_registers_stack_and_tls() {
    let mut scenario = open_core("crash-gcc-o0-segv.core");
    let (stop, snapshot) = stopped(&mut scenario).await;
    let spin_line = crash_source_line("while (atomic_load(&release) == 0) {");
    let mut stack_pointers = vec![register(
        &scenario
            .operation("registers", scenario.handle().registers())
            .await,
        RegisterRole::StackPointer,
    )];
    let mut workers = Vec::new();
    for thread in snapshot
        .threads
        .iter()
        .filter(|thread| thread.id != stop.thread)
    {
        scenario
            .operation("select", scenario.handle().select_thread(thread.id))
            .await;
        let selected = scenario.snapshot().await;
        assert_eq!(selected.selected_thread, Some(thread.id));
        assert_eq!(
            selected.stop_id,
            Some(stop.stop),
            "selection never changes the stop"
        );

        let location = scenario
            .operation("location", scenario.handle().current_location())
            .await;
        assert_eq!(
            location
                .image
                .function
                .as_ref()
                .map(|function| function.name.as_ref()),
            Some("worker_spin")
        );
        assert_eq!(
            location
                .image
                .source
                .as_ref()
                .map(|source| source.line.get()),
            Some(spin_line)
        );
        let worker = signed(&variable(&scenario, "local_id").await);
        assert_eq!(
            signed(&variable(&scenario, "squared").await),
            worker * worker
        );
        assert_eq!(
            signed(&variable(&scenario, "crash_tls").await),
            100 + worker
        );
        assert_eq!(
            signed(&variable(&scenario, "crash_library_tls").await),
            654 + worker
        );
        workers.push(worker);
        stack_pointers.push(register(
            &scenario
                .operation("registers", scenario.handle().registers())
                .await,
            RegisterRole::StackPointer,
        ));

        let (trace, names) = named_frames(&scenario).await;
        assert_eq!(names[0].1.as_deref(), Some("worker_spin"));
        assert_eq!(names[1].1.as_deref(), Some("worker_main"));
        assert_eq!(trace.termination, UnwindTermination::Complete, "{names:#?}");
    }
    workers.sort_unstable();
    assert_eq!(workers, [1, 2, 3]);
    stack_pointers.sort_unstable();
    stack_pointers.dedup();
    assert_eq!(stack_pointers.len(), 4, "each thread has its own stack");

    // Returning to the faulting thread restores its own frame and values.
    scenario
        .operation("select", scenario.handle().select_thread(stop.thread))
        .await;
    assert_eq!(signed(&variable(&scenario, "crash_tls").await), 100);
    assert!(matches!(
        scenario
            .handle()
            .select_thread(uscope::ThreadId::new(1))
            .await,
        Err(Error::UnknownThread(thread)) if thread.get() == 1
    ));
    scenario.shutdown().await;
}

#[tokio::test]
async fn post_mortem_targets_reject_execution_modification_and_breakpoints() {
    let mut scenario = open_core("crash-gcc-o0-segv.core");
    let (stop, before) = stopped(&mut scenario).await;
    let handle = scenario.handle().clone();
    let pc = handle.current_location().await.unwrap().address;

    let rejected = [
        ("launch", handle.launch().await.map(drop)),
        ("run", handle.run().await.map(drop)),
        ("continue", handle.resume().await.map(drop)),
        (
            "continue thread",
            handle
                .continue_execution(
                    stop.stop,
                    ResumeScope::Thread(stop.thread),
                    ExceptionDisposition::Suppress,
                )
                .await
                .map(drop),
        ),
        ("pause", handle.pause().await.map(drop)),
        ("step", handle.step(StepKind::Instruction).await.map(drop)),
        ("next", handle.step(StepKind::OverSource).await.map(drop)),
        ("finish", handle.step(StepKind::Out).await.map(drop)),
        (
            "attach",
            handle.attach_process(stop.process).await.map(drop),
        ),
        ("write", handle.write_word(pc, 0).await),
        (
            "break",
            handle
                .add_breakpoint(uscope::BreakpointSpec::Function("main".to_owned()))
                .await
                .map(drop),
        ),
        (
            "delete",
            handle
                .remove_breakpoint(uscope::BreakpointId::new(1))
                .await
                .map(drop),
        ),
        (
            "delete all",
            handle.remove_all_breakpoints().await.map(drop),
        ),
    ];
    for (operation, result) in rejected {
        assert!(
            matches!(result, Err(Error::PostMortemTarget)),
            "{operation} must be rejected: {result:?}"
        );
    }

    // Nothing changed: the same stop, revision, and memory remain inspectable.
    let (after_stop, after) = stopped(&mut scenario).await;
    assert_eq!(after.revision, before.revision);
    assert_eq!(after_stop.stop, stop.stop);
    assert!(after.breakpoints.is_empty());
    let original = handle.read_word(pc).await.unwrap();
    assert_eq!(handle.read_word(pc).await.unwrap(), original);
    scenario.drain_pending_events();
    assert_eq!(
        scenario.last_revision(),
        0,
        "a core session publishes no events"
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn module_mismatches_are_fatal_unless_explicitly_allowed() {
    // A rebuild with the same layout differs only in content and build-id.
    let rebuilt = Debugger::open_core(&options(
        "crash-gcc-o0-segv.core",
        Some("crash-gcc-o0-rebuilt"),
        false,
    ));
    assert!(
        matches!(&rebuilt, Err(Error::CoreModuleMismatch { path, detail })
            if path.ends_with("crash-gcc-o0-rebuilt") && detail.contains("build-id")),
        "{:?}",
        rebuilt.err()
    );
    // Without a build-id, saved read-only bytes expose a different program.
    for (core_name, executable) in [
        ("crash-clang-o2-segv.core", "crash-gcc-o2-nopie"),
        ("crash-gcc-o0-segv.core", "crash-clang-o2"),
        ("crash-clang-o2-segv.core", "fatal-signal"),
    ] {
        let result = Debugger::open_core(&options(core_name, Some(executable), false));
        assert!(
            matches!(&result, Err(Error::CoreModuleMismatch { path, .. }) if path.ends_with(executable)),
            "{core_name} with {executable}: {:?}",
            result.err()
        );
    }

    // Allowing the mismatch uses the file's metadata but never its contents:
    // the dump did not save .rodata, which only a verified file may supply.
    let verified = open_core("crash-gcc-o0-segv.core");
    let message = verified
        .operation(
            "message",
            verified.handle().runtime_address("crash_message"),
        )
        .await;
    let verified_message = verified
        .operation("read message", verified.handle().read_memory(message, 16))
        .await;
    assert_eq!(verified_message.completion, MemoryReadCompletion::Complete);
    verified.shutdown().await;

    let allowed = Scenario::open_core(
        "allowed mismatch",
        &options("crash-gcc-o0-segv.core", Some("crash-gcc-o0-rebuilt"), true),
    );
    let info = core_info(&allowed);
    assert!(matches!(
        &info.modules[0].state,
        CoreModuleState::Loaded { identity: ModuleIdentity::Mismatched { detail }, .. }
            if detail.contains("build-id")
    ));
    assert_eq!(
        allowed
            .operation("message", allowed.handle().runtime_address("crash_message"))
            .await,
        message,
        "the rebuilt file has the same layout"
    );
    let unbacked = allowed
        .operation("read message", allowed.handle().read_memory(message, 16))
        .await;
    assert!(unbacked.bytes.is_empty(), "{unbacked:?}");
    assert!(matches!(
        unbacked.completion,
        MemoryReadCompletion::Incomplete { .. }
    ));
    // Libraries that do match remain verified and keep their file contents.
    assert_eq!(loaded_identity(&info, "libc.so"), &ModuleIdentity::BuildId);
    allowed.shutdown().await;
}

#[tokio::test]
async fn unverifiable_modules_require_permission_and_expose_only_registers() {
    let refused = Debugger::open_core(&options("crash-gcc-o0-memoryless.core", None, false));
    assert!(
        matches!(&refused, Err(Error::CoreModuleUnverified { path }) if path.ends_with("crash-gcc-o0")),
        "{:?}",
        refused.err()
    );

    let mut scenario = Scenario::open_core(
        "memoryless",
        &options("crash-gcc-o0-memoryless.core", None, true),
    );
    let info = core_info(&scenario);
    assert!(info.modules.len() >= 4, "{info:#?}");
    assert!(info.modules.iter().all(|module| matches!(
        &module.state,
        CoreModuleState::Loaded {
            identity: ModuleIdentity::Unverified,
            ..
        }
    )));
    let (_, snapshot) = stopped(&mut scenario).await;
    assert_eq!(snapshot.threads.len(), 4);

    // Registers survive in notes; every byte of memory is unavailable.
    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;
    assert_eq!(
        location
            .image
            .function
            .as_ref()
            .map(|function| function.name.as_ref()),
        Some("crash_segv")
    );
    let (trace, _) = named_frames(&scenario).await;
    assert_eq!(trace.frames.len(), 1);
    assert!(
        matches!(
            trace.termination,
            UnwindTermination::MemoryReadFailed { .. }
        ),
        "{trace:?}"
    );
    let depth = variable(&scenario, "depth").await;
    assert!(
        matches!(
            depth.state,
            VariableState::Unavailable(VariableUnavailableReason::MemoryInaccessible { .. })
        ),
        "{depth:?}"
    );
    let text = scenario
        .operation(
            "read text",
            scenario.handle().read_memory(location.address, 1),
        )
        .await;
    assert!(matches!(
        text.completion,
        MemoryReadCompletion::Incomplete { .. }
    ));
    scenario.shutdown().await;
}

#[tokio::test]
async fn header_pages_omitted_from_a_dump_are_recovered_from_verified_files() {
    let scenario = open_core("crash-gcc-o0-headerless.core");
    let info = core_info(&scenario);
    for module in ["crash-gcc-o0", "libc.so", "libcrash.so"] {
        assert!(
            matches!(loaded_identity(&info, module), ModuleIdentity::SavedContent { compared_bytes } if *compared_bytes > 0),
            "{module}: {info:#?}"
        );
    }
    let (_, names) = named_frames(&scenario).await;
    assert_eq!(names[1].1.as_deref(), Some("main"));
    assert!(names[2].0.starts_with("libc.so"), "{names:#?}");
    assert_eq!(
        signed(&variable(&scenario, "crash_library_value").await),
        321
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn missing_files_are_reported_and_leave_their_images_unavailable() {
    let scenario = open_core("core-missing-library/crash.core");
    let info = core_info(&scenario);
    let missing = info
        .modules
        .iter()
        .find(|module| {
            module
                .recorded_path
                .ends_with("core-missing-library/libcrash.so")
        })
        .unwrap_or_else(|| panic!("the deleted library is not reported: {info:#?}"));
    assert_eq!(missing.state, CoreModuleState::Missing);
    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    assert!(
        modules
            .modules
            .iter()
            .all(|module| !module.path.ends_with("libcrash.so"))
    );
    assert!(matches!(
        scenario.handle().variable("crash_library_value").await,
        Err(Error::VariableNotFound(_))
    ));
    // The executable's own state is unaffected.
    assert_eq!(signed(&variable(&scenario, "depth").await), 3);
    assert_eq!(signed(&variable(&scenario, "crash_tls").await), 100);
    scenario.shutdown().await;

    let unavailable = Debugger::open_core(&CoreDumpOptions::new(core(
        "core-missing-executable/crash.core",
    )));
    assert!(
        matches!(&unavailable, Err(Error::CoreExecutableUnavailable(message)) if message.contains("supply the executable")),
        "{:?}",
        unavailable.err()
    );
    let explicit = Scenario::open_core(
        "explicit executable",
        &options(
            "core-missing-executable/crash.core",
            Some("crash-gcc-o0"),
            false,
        ),
    );
    assert_eq!(
        loaded_identity(&core_info(&explicit), "crash-gcc-o0"),
        &ModuleIdentity::BuildId
    );
    assert_eq!(signed(&variable(&explicit, "depth").await), 3);
    explicit.shutdown().await;
}

/// Edits a copy of a core's ELF header or program headers. The copy lives
/// until the returned directory is dropped.
fn edited_core(test: &str, source: &str, edit: impl FnOnce(&mut Vec<u8>)) -> (ScratchDir, PathBuf) {
    let mut bytes = fs::read(core(source)).expect("read core fixture");
    edit(&mut bytes);
    let directory = ScratchDir::new(&format!("post-mortem-{test}"));
    let path = directory.path().join("edited.core");
    fs::write(&path, bytes).expect("write edited core");
    (directory, path)
}

/// Returns the file offset of each `PT_LOAD` header and the address range it maps.
fn load_headers(bytes: &[u8]) -> Vec<(usize, u64, u64)> {
    let header = elf::FileHeader64::<Endianness>::parse(bytes).expect("core header");
    let endian = header.endian().expect("core endianness");
    let table = usize::try_from(header.e_phoff.get(endian)).unwrap();
    let size = usize::from(header.e_phentsize.get(endian));
    header
        .program_headers(endian, bytes)
        .expect("program headers")
        .iter()
        .enumerate()
        .filter(|(_, program)| program.p_type(endian) == elf::PT_LOAD)
        .map(|(index, program)| {
            (
                table + index * size,
                program.p_vaddr(endian),
                program.p_vaddr(endian) + program.p_memsz(endian),
            )
        })
        .collect()
}

/// Returns the file range of the core's note segment.
fn note_segment(bytes: &[u8]) -> (usize, usize) {
    let header = elf::FileHeader64::<Endianness>::parse(bytes).expect("core header");
    let endian = header.endian().expect("core endianness");
    header
        .program_headers(endian, bytes)
        .expect("program headers")
        .iter()
        .find(|program| program.p_type(endian) == elf::PT_NOTE)
        .map(|program| {
            (
                usize::try_from(program.p_offset(endian)).unwrap(),
                usize::try_from(program.p_filesz(endian)).unwrap(),
            )
        })
        .expect("a note segment")
}

const PHDR_OFFSET: usize = 8;
/// Rewrites one program header, given the file offset of that header.
type SegmentEdit = fn(&mut Vec<u8>, usize);
const PHDR_FILESZ: usize = 32;

#[tokio::test]
async fn unsaved_and_truncated_memory_stays_unavailable_rather_than_guessed() {
    // Locate the faulting thread's stack segment in an intact dump.
    let reference = open_core("crash-gcc-o0-segv.core");
    let stack = register(
        &reference
            .operation("registers", reference.handle().registers())
            .await,
        RegisterRole::StackPointer,
    );
    reference.shutdown().await;

    let edits: [(&str, SegmentEdit); 2] = [
        // The kernel's form for pages it chose not to save.
        ("unsaved", |bytes, header| {
            bytes[header + PHDR_FILESZ..header + PHDR_FILESZ + 8]
                .copy_from_slice(&0_u64.to_le_bytes());
        }),
        // A dump truncated before the segment's data.
        ("truncated", |bytes, header| {
            let past_end = u64::try_from(bytes.len()).unwrap() + 4096;
            bytes[header + PHDR_OFFSET..header + PHDR_OFFSET + 8]
                .copy_from_slice(&past_end.to_le_bytes());
        }),
    ];
    for (name, edit) in edits {
        let (_scratch, path) = edited_core(name, "crash-gcc-o0-segv.core", |bytes| {
            let (header, _, _) = load_headers(bytes)
                .into_iter()
                .find(|&(_, start, end)| (start..end).contains(&stack))
                .expect("a load segment holds the stack");
            edit(bytes, header);
        });
        let scenario = Scenario::open_core(name, &CoreDumpOptions::new(path));
        let registers = scenario
            .operation("registers", scenario.handle().registers())
            .await;
        assert_eq!(register(&registers, RegisterRole::StackPointer), stack);
        let (trace, _) = named_frames(&scenario).await;
        assert_eq!(trace.frames.len(), 1, "{name}: {trace:?}");
        assert!(
            matches!(
                trace.termination,
                UnwindTermination::MemoryReadFailed { .. }
            ),
            "{name}"
        );
        let depth = variable(&scenario, "depth").await;
        assert!(
            matches!(
                depth.state,
                VariableState::Unavailable(VariableUnavailableReason::MemoryInaccessible { .. })
            ),
            "{name}: {depth:?}"
        );
        // Memory outside the edited segment is unaffected.
        assert_eq!(
            signed(&variable(&scenario, "crash_counter").await),
            8,
            "{name}"
        );
        scenario.shutdown().await;
    }
}

/// Reads `length` bytes and returns them only when the read completed.
async fn complete_read(
    scenario: &Scenario,
    address: VirtualAddress,
    length: u64,
) -> Option<Vec<u8>> {
    let read = scenario
        .operation("read", scenario.handle().read_memory(address, length))
        .await;
    if read.completion == MemoryReadCompletion::Complete {
        Some(read.bytes.to_vec())
    } else {
        assert!(read.bytes.is_empty(), "{read:?}");
        None
    }
}

#[tokio::test]
async fn modified_file_pages_missing_from_a_dump_are_never_read_from_the_file() {
    let message_bytes = b"post-mortem read-only message\0";
    // Each core is a separate randomized run with its own load addresses.
    let addresses = async |scenario: &Scenario| {
        let handle = scenario.handle();
        (
            scenario
                .operation("counter", handle.runtime_address("crash_counter"))
                .await,
            scenario
                .operation("message", handle.runtime_address("crash_message"))
                .await,
        )
    };
    let reference = open_core("crash-gcc-o0-segv.core");
    let (counter, message) = addresses(&reference).await;
    assert_eq!(
        complete_read(&reference, counter, 4).await,
        Some(8_i32.to_le_bytes().to_vec()),
        "the dump saved the incremented counter"
    );
    reference.shutdown().await;

    // A dump that saved only header pages omits the modified .data page too.
    // The file still holds the counter's initial value, which is not the
    // process's memory.
    let headers_only = open_core("crash-gcc-o0-headers-only.core");
    assert_eq!(
        loaded_identity(&core_info(&headers_only), "crash-gcc-o0"),
        &ModuleIdentity::BuildId
    );
    let (headers_counter, headers_message) = addresses(&headers_only).await;
    assert_eq!(
        complete_read(&headers_only, headers_message, message_bytes.len() as u64).await,
        Some(message_bytes.to_vec()),
        "read-only pages are recovered from the verified file"
    );
    assert_eq!(complete_read(&headers_only, headers_counter, 4).await, None);
    headers_only.shutdown().await;

    // A dump cut short inside the saved .data segment loses the counter. Its
    // header records that the producer saved those bytes, so the file cannot
    // stand in for them. gcore writes notes last, so the segment's data is
    // moved to end at the counter instead of truncating the file itself.
    let (_scratch, path) = edited_core("truncated-data", "crash-gcc-o0-segv.core", |bytes| {
        let (header, start, _) = load_headers(bytes)
            .into_iter()
            .find(|&(_, start, end)| (start..end).contains(&counter.get()))
            .expect("a load segment holds the counter");
        let cut = u64::try_from(bytes.len()).unwrap() - (counter.get() - start);
        bytes[header + PHDR_OFFSET..header + PHDR_OFFSET + 8].copy_from_slice(&cut.to_le_bytes());
    });
    let truncated = Scenario::open_core("truncated data", &CoreDumpOptions::new(path));
    assert_eq!(
        loaded_identity(&core_info(&truncated), "crash-gcc-o0"),
        &ModuleIdentity::BuildId
    );
    assert_eq!(
        complete_read(&truncated, message, message_bytes.len() as u64).await,
        Some(message_bytes.to_vec())
    );
    assert_eq!(complete_read(&truncated, counter, 4).await, None);
    truncated.shutdown().await;
}

const PHDR_VADDR: usize = 16;

/// Writes a copy of the gcc -O0 crash executable with edited program headers.
/// The copy lives until the returned directory is dropped.
fn edited_executable(test: &str, edit: impl FnOnce(&mut Vec<u8>)) -> (ScratchDir, PathBuf) {
    let mut bytes = fs::read(Scenario::fixture("crash-gcc-o0")).expect("read executable");
    edit(&mut bytes);
    let directory = ScratchDir::new(&format!("post-mortem-{test}"));
    let path = directory.path().join("crash-gcc-o0");
    fs::write(&path, bytes).expect("write edited executable");
    (directory, path)
}

fn with_executable(executable: &Path, allow: bool) -> CoreDumpOptions {
    CoreDumpOptions {
        core: core("crash-gcc-o0-segv.core"),
        executable: Some(executable.to_owned()),
        allow_module_mismatch: allow,
    }
}

#[tokio::test]
async fn allowed_mismatches_keep_the_recorded_placement_or_refuse_to_relocate() {
    let reference = open_core("crash-gcc-o0-segv.core");
    let message = reference
        .operation(
            "message",
            reference.handle().runtime_address("crash_message"),
        )
        .await;
    reference.shutdown().await;

    // A segment extending past the end of the file proves a different file,
    // but the recorded mapping still places it.
    let (_oversized_scratch, oversized) = edited_executable("oversized", |bytes| {
        let (header, _, _) = *load_headers(bytes).last().expect("a load segment");
        let past_end = u64::try_from(bytes.len()).unwrap() + 1;
        bytes[header + PHDR_FILESZ..header + PHDR_FILESZ + 8]
            .copy_from_slice(&past_end.to_le_bytes());
    });
    assert!(matches!(
        Debugger::open_core(&with_executable(&oversized, false)),
        Err(Error::CoreModuleMismatch { detail, .. }) if detail.contains("outside")
    ));
    let allowed = Scenario::open_core("oversized", &with_executable(&oversized, true));
    assert!(matches!(
        loaded_identity(&core_info(&allowed), "crash-gcc-o0"),
        ModuleIdentity::Mismatched { detail } if detail.contains("outside")
    ));
    assert_eq!(
        allowed
            .operation("message", allowed.handle().runtime_address("crash_message"))
            .await,
        message,
        "the module is relocated to its recorded mapping"
    );
    allowed.shutdown().await;

    // Linked above the recorded image, the file has no load bias that places
    // it there. Relocating it anywhere would be a guess.
    let (_unplaceable_scratch, unplaceable) = edited_executable("unplaceable", |bytes| {
        for (header, _, _) in load_headers(bytes) {
            let address = u64::from_le_bytes(
                bytes[header + PHDR_VADDR..header + PHDR_VADDR + 8]
                    .try_into()
                    .unwrap(),
            );
            bytes[header + PHDR_VADDR..header + PHDR_VADDR + 8]
                .copy_from_slice(&(address + 0x7f00_0000_0000).to_le_bytes());
        }
    });
    assert!(matches!(
        Debugger::open_core(&with_executable(&unplaceable, false)),
        Err(Error::CoreModuleMismatch { .. })
    ));
    let refused = Debugger::open_core(&with_executable(&unplaceable, true));
    assert!(
        matches!(&refused, Err(Error::CoreModuleUnplaceable { path, detail })
            if path.ends_with("crash-gcc-o0") && detail.contains("no loadable segment")),
        "{:?}",
        refused.err()
    );
}

#[tokio::test]
async fn invalid_core_files_fail_with_typed_errors() {
    let scratch = ScratchDir::new("post-mortem-invalid");
    let directory = scratch.path();
    let open = |path: PathBuf| Debugger::open_core(&CoreDumpOptions::new(path));
    let invalid = |result: uscope::Result<Debugger>, expected: &str| match result {
        Err(Error::InvalidCoreDump(message)) => {
            assert!(message.contains(expected), "{message:?} lacks {expected:?}");
        }
        other => panic!("expected an invalid core ({expected}): {:?}", other.err()),
    };

    assert!(matches!(
        open(directory.join("absent.core")),
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound
    ));
    let empty = directory.join("empty.core");
    fs::write(&empty, []).unwrap();
    invalid(open(empty), "not a 64-bit ELF file");
    let garbage = directory.join("garbage.core");
    fs::write(
        &garbage,
        (0..4096_u32)
            .map(|value| (value * 31 % 251) as u8)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    invalid(open(garbage), "not a 64-bit ELF file");
    invalid(open(Scenario::fixture("crash-gcc-o0")), "not a core dump");

    let source = fs::read(core("crash-gcc-o0-segv.core")).unwrap();
    let truncated = directory.join("truncated.core");
    fs::write(&truncated, &source[..64]).unwrap();
    invalid(open(truncated), "program headers are malformed");
    // Truncation inside the note segment loses thread state, which must
    // never be reconstructed.
    let (notes_offset, notes_size) = note_segment(&source);
    let headless = directory.join("notes-truncated.core");
    fs::write(&headless, &source[..notes_offset + notes_size / 2]).unwrap();
    invalid(open(headless), "note segment is malformed");

    invalid(
        open(
            edited_core("machine", "crash-gcc-o0-segv.core", |bytes| {
                bytes[18..20].copy_from_slice(&elf::EM_AARCH64.to_le_bytes());
            })
            .1,
        ),
        "only x86-64",
    );
    invalid(
        open(
            edited_core("overlap", "crash-gcc-o0-segv.core", |bytes| {
                let headers = load_headers(bytes);
                let (_, second_start, _) = headers[1];
                let (first, _, _) = headers[0];
                // Grow the first segment's memory size over the second.
                let start = u64::from_le_bytes(bytes[first + 16..first + 24].try_into().unwrap());
                let size = second_start - start + 4096;
                bytes[first + 40..first + 48].copy_from_slice(&size.to_le_bytes());
            })
            .1,
        ),
        "overlap",
    );
    assert!(matches!(
        Debugger::open_core(&options("crash-gcc-o0-segv.core", Some("absent-executable"), false)),
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound
    ));
}

/// A core file, its innermost expected functions, and selected locals.
type LanguageCore = (
    &'static str,
    &'static [&'static str],
    &'static [(&'static str, i128)],
);

#[tokio::test]
async fn go_rust_and_zig_cores_present_frames_and_locals() {
    let cases: [LanguageCore; 3] = [
        (
            "crash-go-o0.core",
            &["main.crashNow", "main.main", "runtime.main"],
            &[("depth", 3), ("doubled", 6)],
        ),
        // Rust -O0 keeps ptr::write_volatile as a physical frame.
        (
            "crash-rust-o0.core",
            &["write_volatile<i32>", "crash_now", "main"],
            &[("src", 6)],
        ),
        (
            "crash-zig-o0.core",
            &["crashNow", "main"],
            &[("depth", 3), ("doubled", 6)],
        ),
    ];
    for (core_name, expected_frames, locals) in cases {
        let mut scenario = open_core(core_name);
        let (stop, _) = stopped(&mut scenario).await;
        assert!(
            matches!(&stop.reason, StopReason::CoreDump { exception: Some(exception) } if exception.code == 11),
            "{core_name}: {:?}",
            stop.reason
        );
        let (trace, names) = named_frames(&scenario).await;
        let functions = names
            .iter()
            .map(|(_, function)| function.as_deref())
            .collect::<Vec<_>>();
        for (index, expected) in expected_frames.iter().enumerate() {
            assert_eq!(functions[index], Some(*expected), "{core_name}: {names:#?}");
        }
        assert_eq!(
            trace.termination,
            UnwindTermination::Complete,
            "{core_name}"
        );
        for (name, value) in locals {
            assert_eq!(
                signed(&variable(&scenario, name).await),
                *value,
                "{core_name}: {name}"
            );
        }
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn core_sessions_coexist_with_each_other_and_a_live_session() {
    let first = open_core("crash-gcc-o0-segv.core");
    let second = open_core("crash-clang-o2-abort.core");
    let mut live = Scenario::new("live", Scenario::fixture("fatal-signal"));
    assert!(
        matches!(live.run_to_stop().await, StopReason::Exception(exception) if exception.code == 11)
    );

    let (first_names, second_names) = tokio::join!(named_frames(&first), named_frames(&second));
    assert_eq!(first_names.1[0].1.as_deref(), Some("crash_segv"));
    assert!(
        second_names
            .1
            .iter()
            .any(|(_, function)| function.as_deref() == Some("crash_abort"))
    );
    let first_stop = first.handle().snapshot().await.unwrap().stop_id;
    let second_stop = second.handle().snapshot().await.unwrap().stop_id;
    assert_ne!(
        first_stop, second_stop,
        "stop identifiers are unique across sessions"
    );

    // Dropping a session without an explicit shutdown releases it cleanly.
    drop(second);
    first.shutdown().await;
    live.shutdown().await;
}
