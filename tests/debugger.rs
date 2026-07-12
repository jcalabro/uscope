mod support;

use uscope::{
    Architecture, BreakpointLocation, ByteOrder, Debugger, Error, ExitStatus, InferiorState,
    PointerWidth, RegisterRole, SourceContext, SourceFile, SourceLocation, StepKind, StopReason,
    ThreadState, UnwindTermination, VirtualAddress,
};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use support::Scenario;
use tokio::time::{Duration, timeout};

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

    let BreakpointLocation::Image(image_address) = breakpoint else {
        panic!("function breakpoint was not image-based")
    };

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
    assert_eq!(snapshot.breakpoints.as_ref(), &[breakpoint]);

    let registers = scenario
        .operation("read registers", scenario.handle().registers())
        .await;

    assert_register_snapshot(&registers, &snapshot, first_address);

    let duplicate = scenario.add_breakpoint("breakpoint_target").await;

    assert_eq!(duplicate, breakpoint);
    assert_eq!(
        scenario.snapshot().await.breakpoints.as_ref(),
        &[breakpoint]
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
    assert_eq!(breakpoint, BreakpointLocation::Virtual(return_address));

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
