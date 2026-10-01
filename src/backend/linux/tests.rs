use crate::CodeInstanceKind;
use crate::ExceptionDisposition;
use crate::InlineFrameLookup;
use crate::MemoryReadCompletion;
use crate::MemoryReadUnavailableReason;
use crate::Path;
use crate::PresentedFrame;
use crate::ValueExpression;
use crate::ValuePathStep;
use crate::VariableQuery;
use crate::WatchpointSpec;
use crate::debug_info::VariableContext;
use crate::debug_info::VariableRuntime;
use crate::inspection::InspectionBudget;
use crate::unwind::FrameContext;
use crate::unwind::MemoryReader;
use crate::unwind::RegisterFile;
use std::cell::RefCell;
use std::rc::Rc;

use super::classify::{WatchStatus, classify_stop_evidence, format_raw_stop};
use super::frames::{default_inline_visible_count, frame_lookup_address};
use super::inspection::validate_value_expression;
use super::memory::{MemoryAccessError, read_logical_memory_with};
use super::modules::{ModuleMapping, parse_maps};
use super::native::{InspectionOps, LinuxTraceOps, ThreadAffinity, queued_trap_in_status};
use super::*;
use crate::{AddressRange, ImageAddress};

#[test]
fn value_expression_validation_bounds_untrusted_request_structure() {
    let valid = ValueExpression {
        steps: std::iter::once(ValuePathStep::Named("root".to_owned()))
            .chain(std::iter::repeat_n(
                ValuePathStep::Dereference,
                MAX_VALUE_EXPRESSION_DEREFERENCES,
            ))
            .collect::<Vec<_>>()
            .into(),
    };
    validate_value_expression(&valid).expect("bounded dereferences");
    let valid = ValueExpression {
        steps: vec![
            ValuePathStep::Named("root".to_owned()),
            ValuePathStep::Named("member".to_owned()),
            ValuePathStep::Index(1),
        ]
        .into(),
    };
    validate_value_expression(&valid).expect("bounded expression");

    for expression in [
        ValueExpression {
            steps: Arc::new([]),
        },
        ValueExpression {
            steps: vec![
                ValuePathStep::Named("root".to_owned()),
                ValuePathStep::Named(String::new()),
            ]
            .into(),
        },
        ValueExpression {
            steps: (0..=MAX_VALUE_EXPRESSION_STEPS)
                .map(|index| ValuePathStep::Named(format!("member{index}")))
                .collect::<Vec<_>>()
                .into(),
        },
        ValueExpression {
            steps: vec![ValuePathStep::Index(0)].into(),
        },
    ] {
        assert!(matches!(
            validate_value_expression(&expression),
            Err(Error::InvalidValueExpression(_))
        ));
    }
}

#[test]
fn maps_parser_preserves_distinct_loads_and_rejects_corruption() {
    let maps = concat!(
        "1000-2000 r--p 00000000 00:01 7 /opt/lib/libsame.so\n",
        "2000-3000 r-xp 00001000 00:01 7 /opt/lib/libsame.so\n",
        "5000-6000 r-xp 00001000 00:01 7 /opt/lib/libsame.so\n",
        "7000-8000 r-xp 00000000 00:01 8 /opt/bin/app (deleted)\n",
        // The kernel pads the pathname column and does not escape spaces
        // within the path itself; both must survive parsing intact.
        "9000-a000 r-xp 00000000 00:01 9     /opt/my libs/libspace.so\n",
        "8000-9000 r-xp 00000000 00:00 0 [vdso]\n",
        "b000-c000 rw-p 00000000 00:00 0\n",
    );
    let mapping = |path: &str, inode, start, file_offset, deleted, executable| ModuleMapping {
        path: PathBuf::from(path),
        inode,
        start,
        file_offset,
        deleted,
        executable,
    };

    assert_eq!(
        parse_maps(maps).expect("valid maps"),
        vec![
            mapping("/opt/bin/app", 8, 0x7000, 0, true, true),
            mapping("/opt/lib/libsame.so", 7, 0x1000, 0, false, false),
            mapping("/opt/lib/libsame.so", 7, 0x2000, 0x1000, false, true),
            mapping("/opt/lib/libsame.so", 7, 0x5000, 0x1000, false, true),
            mapping("/opt/my libs/libspace.so", 9, 0x9000, 0, false, true),
        ]
    );
    assert!(parse_maps("not-a-mapping").is_err());
}

#[test]
fn logical_memory_reads_unaligned_cross_word_ranges_and_hides_traps() {
    let address = VirtualAddress::new(0x1003);
    let mut breakpoints = BTreeMap::new();
    let mut reads = Vec::new();
    breakpoints.insert(
        VirtualAddress::new(0x1005),
        BreakpointSite {
            original_byte: 0x55,
            installed: true,
            owners: BTreeSet::new(),
        },
    );
    breakpoints.insert(
        VirtualAddress::new(0x1008),
        BreakpointSite {
            original_byte: 0x88,
            installed: true,
            owners: BTreeSet::new(),
        },
    );
    breakpoints.insert(
        VirtualAddress::new(0x1009),
        BreakpointSite {
            original_byte: 0x99,
            installed: false,
            owners: BTreeSet::new(),
        },
    );
    breakpoints.insert(
        VirtualAddress::new(0x1010),
        BreakpointSite {
            original_byte: 0x10,
            installed: true,
            owners: BTreeSet::new(),
        },
    );
    let bytes = read_logical_memory_with(address, 10, &breakpoints, |current| {
        reads.push(current);
        let mut bytes = [0_u8; 8];
        for (offset, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::try_from(current + offset as u64 - 0x1000).expect("test byte fits u8");
        }
        for (&site_address, site) in &breakpoints {
            let Some(offset) = site_address.get().checked_sub(current) else {
                continue;
            };
            if site.installed && offset < bytes.len() as u64 {
                bytes[usize::try_from(offset).expect("test offset fits usize")] = BREAKPOINT_OPCODE;
            }
        }
        Ok(u64::from_le_bytes(bytes))
    })
    .expect("logical memory read");
    assert_eq!(bytes.bytes, [3, 4, 0x55, 6, 7, 0x88, 9, 10, 11, 12]);
    assert_eq!(bytes.completion, MemoryReadCompletion::Complete);
    assert_eq!(reads, [0x1000, 0x1008]);
    let empty = read_logical_memory_with(address, 0, &breakpoints, |_| {
        panic!("an empty logical read must not read target memory")
    })
    .expect("empty logical memory read");
    assert!(empty.bytes.is_empty());
    assert_eq!(empty.completion, MemoryReadCompletion::Complete);
}

#[test]
fn logical_memory_reads_return_the_prefix_before_inaccessible_memory() {
    let mut reads = Vec::new();
    let result = read_logical_memory_with(
        VirtualAddress::new(0x1003),
        10,
        &BTreeMap::new(),
        |current| {
            reads.push(current);
            if current == 0x1008 {
                return Err(MemoryAccessError::Inaccessible);
            }
            Ok(u64::from_le_bytes([0, 1, 2, 3, 4, 5, 6, 7]))
        },
    )
    .expect("inaccessible memory is a typed partial result");

    assert_eq!(result.bytes, [3, 4, 5, 6, 7]);
    assert_eq!(
        result.completion,
        MemoryReadCompletion::Incomplete {
            next_address: VirtualAddress::new(0x1008),
            reason: MemoryReadUnavailableReason::Inaccessible,
        }
    );
    assert_eq!(reads, [0x1000, 0x1008]);
}

#[test]
fn logical_memory_reads_do_not_disguise_operational_failures_as_inaccessible() {
    let result = read_logical_memory_with(VirtualAddress::new(0x1000), 8, &BTreeMap::new(), |_| {
        Err(MemoryAccessError::Fatal(Error::RequestCancelled))
    });

    assert!(matches!(result, Err(Error::RequestCancelled)));
}

struct RecordingTrace {
    actions: Rc<RefCell<Vec<&'static str>>>,
    pid: Pid,
}

impl RecordingTrace {
    fn record(&self, action: &'static str) {
        self.actions.borrow_mut().push(action);
    }

    fn unexpected<T>(operation: &str) -> T {
        panic!("unexpected native operation: {operation}")
    }
}

impl InspectionOps for RecordingTrace {
    fn read_word(&self, _pid: Pid, _address: u64) -> Result<u64> {
        Self::unexpected("read_word")
    }

    fn registers(&self, pid: Pid) -> Result<libc::user_regs_struct> {
        assert_eq!(pid, self.pid);
        self.record("registers");
        Ok(libc::user_regs_struct {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            rbp: 0,
            rbx: 0,
            r11: 0,
            r10: 0,
            r9: 0,
            r8: 0,
            rax: 0,
            rcx: 0,
            rdx: 0,
            rsi: 0,
            rdi: 0,
            orig_rax: 0,
            rip: 0x5000,
            cs: 0,
            eflags: 0,
            rsp: 0,
            ss: 0,
            fs_base: 0,
            gs_base: 0,
            ds: 0,
            es: 0,
            fs: 0,
            gs: 0,
        })
    }
}

impl LinuxTraceOps for RecordingTrace {
    fn spawn(&self, _executable: &Path) -> Result<Pid> {
        self.record("spawn");
        Ok(self.pid)
    }

    fn spawn_waiter(&self, _messages: mpsc::Sender<ControllerMessage>) -> Result<Waiter> {
        self.record("spawn_waiter");
        Ok(Waiter {
            stop: Arc::new(AtomicBool::new(false)),
            thread: thread::spawn(|| {}),
        })
    }

    fn process_threads(&self, _process: Pid) -> Result<Vec<Pid>> {
        Self::unexpected("process_threads")
    }

    fn seize(&self, _pid: Pid) -> Result<bool> {
        Self::unexpected("seize")
    }

    fn interrupt(&self, _pid: Pid) -> Result<bool> {
        Self::unexpected("interrupt")
    }

    fn detach(&self, _pid: Pid, _signal: Option<NixSignal>) -> Result<()> {
        Self::unexpected("detach")
    }

    fn kill(&self, pid: Pid, signal: NixSignal) -> Result<()> {
        assert_eq!(pid, self.pid);
        assert_eq!(signal, NixSignal::SIGKILL);
        self.record("kill");
        Ok(())
    }

    fn reap(&self, _pid: Pid) -> Result<()> {
        Self::unexpected("reap")
    }

    fn thread_group_id(&self, _pid: Pid) -> Result<Pid> {
        Self::unexpected("thread_group_id")
    }

    fn load_bias(
        &self,
        pid: Pid,
        _executable: &Path,
        _executable_data: &[u8],
        _identity: FileIdentity,
    ) -> Result<u64> {
        assert_eq!(pid, self.pid);
        self.record("load_bias");
        Ok(0x5000)
    }

    fn write_word(&self, _pid: Pid, _address: u64, _value: u64) -> Result<()> {
        Self::unexpected("write_word")
    }

    fn continue_execution(&self, pid: Pid, signal: Option<NixSignal>) -> Result<()> {
        assert_eq!(pid, self.pid);
        assert_eq!(signal, None);
        self.record("continue");
        Ok(())
    }

    fn continue_during_shutdown(&self, _pid: Pid) -> Result<()> {
        Self::unexpected("continue_during_shutdown")
    }

    fn step(&self, _pid: Pid, _signal: Option<NixSignal>) -> Result<()> {
        Self::unexpected("step")
    }

    fn set_registers(&self, _pid: Pid, _registers: libc::user_regs_struct) -> Result<()> {
        Self::unexpected("set_registers")
    }

    fn set_options(&self, pid: Pid, _exit_kill: bool) -> Result<()> {
        assert_eq!(pid, self.pid);
        self.record("set_options");
        Ok(())
    }

    fn event_message(&self, _pid: Pid) -> Result<libc::c_long> {
        Self::unexpected("event_message")
    }

    fn signal_metadata(&self, _pid: Pid) -> std::result::Result<SignalMetadata, Errno> {
        Self::unexpected("signal_metadata")
    }

    fn request_stop(&self, _process: Pid, _thread: Pid) -> Result<()> {
        Self::unexpected("request_stop")
    }

    fn read_debug_register(&self, _pid: Pid, _index: usize) -> std::result::Result<u64, Errno> {
        Self::unexpected("read_debug_register")
    }

    fn write_debug_register(
        &self,
        _pid: Pid,
        _index: usize,
        _value: u64,
    ) -> std::result::Result<(), Errno> {
        Self::unexpected("write_debug_register")
    }

    fn install_breakpoint(
        &self,
        _pid: Pid,
        _sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        _address: VirtualAddress,
        _owner: BreakpointOwner,
    ) -> Result<()> {
        Self::unexpected("install_breakpoint")
    }

    fn remove_breakpoint(
        &self,
        _pid: Pid,
        _sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        _address: VirtualAddress,
    ) -> Result<()> {
        Self::unexpected("remove_breakpoint")
    }

    fn reinstall_breakpoint(
        &self,
        _pid: Pid,
        _sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        _address: VirtualAddress,
    ) -> Result<()> {
        Self::unexpected("reinstall_breakpoint")
    }
}

/// Builds a controller over a fake trace, with placeholder unwind and
/// variable providers, and subscribes to its events.
fn test_controller<P: InspectionOps>(
    lease: SessionLease,
    path: &str,
    data: Arc<[u8]>,
    image: Arc<ModuleImage>,
    trace: P,
    event_capacity: usize,
) -> (Controller<P>, broadcast::Receiver<DebuggerEvent>) {
    let (sender, receiver) = mpsc::channel(8);
    let (events, event_receiver) = broadcast::channel(event_capacity);
    let controller = Controller::new(
        lease,
        ExecutableSource {
            display_path: Arc::new(PathBuf::from(path)),
            data,
            identity: FileIdentity { inode: 0 },
            process_start_time: None,
        },
        DebugInfo {
            image,
            unwind: Arc::new(UnusedUnwindInfo),
            variables: Arc::new(UnusedVariableInfo),
        },
        ControllerChannels {
            sender,
            receiver,
            events,
        },
        trace,
    );
    (controller, event_receiver)
}

struct UnusedUnwindInfo;

impl UnwindInfo for UnusedUnwindInfo {
    fn cfa(
        &self,
        _address: ImageAddress,
        _registers: &RegisterFile,
        _memory: &mut dyn MemoryReader,
    ) -> std::result::Result<VirtualAddress, UnwindTermination> {
        panic!("unexpected cfa lookup")
    }

    fn unwind(
        &self,
        _address: ImageAddress,
        _registers: &RegisterFile,
        _memory: &mut dyn MemoryReader,
    ) -> std::result::Result<crate::unwind::UnwindStep, UnwindTermination> {
        RecordingTrace::unexpected("unwind")
    }
}

struct UnusedVariableInfo;

impl VariableInfo for UnusedVariableInfo {
    fn inspect(
        &self,
        _address: ImageAddress,
        _selected: Option<crate::CodeInstanceId>,
        _query: &VariableQuery,
        _context: VariableContext,
        _runtime: &mut dyn VariableRuntime,
        _budget: &mut InspectionBudget,
    ) -> Result<Vec<crate::Variable>> {
        panic!("unexpected variable lookup")
    }

    fn inspect_path(
        &self,
        _address: ImageAddress,
        _selected: Option<crate::CodeInstanceId>,
        _root: &str,
        _selectors: &[crate::ValuePathStep],
        _context: VariableContext,
        _runtime: &mut dyn VariableRuntime,
        _budget: &mut InspectionBudget,
    ) -> Result<crate::InspectedValue> {
        panic!("unexpected variable path lookup")
    }

    fn local_storage(
        &self,
        _address: ImageAddress,
        _selected: Option<crate::CodeInstanceId>,
        _root: &str,
    ) -> Result<crate::debug_info::ObjectStorage> {
        panic!("unexpected local storage lookup")
    }

    fn global_storage(
        &self,
        _id: crate::GlobalVariableId,
    ) -> Result<crate::debug_info::ObjectStorage> {
        panic!("unexpected global storage lookup")
    }

    fn inspect_global(
        &self,
        _id: crate::GlobalVariableId,
        _address: Option<ImageAddress>,
        _context: VariableContext,
        _runtime: &mut dyn VariableRuntime,
        _budget: &mut InspectionBudget,
    ) -> Result<crate::Variable> {
        panic!("unexpected global variable lookup")
    }

    fn inspect_global_path(
        &self,
        _id: crate::GlobalVariableId,
        _address: Option<ImageAddress>,
        _selectors: &[crate::ValuePathStep],
        _context: VariableContext,
        _runtime: &mut dyn VariableRuntime,
        _budget: &mut InspectionBudget,
    ) -> Result<crate::InspectedValue> {
        panic!("unexpected global variable path lookup")
    }

    fn dereference(
        &self,
        _reference: &crate::DereferenceReference,
        _runtime: &mut dyn VariableRuntime,
        _budget: &mut InspectionBudget,
    ) -> Result<crate::DereferencedValue> {
        panic!("unexpected dereference")
    }

    fn value_children(
        &self,
        _reference: &crate::ValueChildrenReference,
        _offset: u64,
        _limit: u32,
        _runtime: &mut dyn VariableRuntime,
        _budget: &mut InspectionBudget,
    ) -> Result<crate::ValueChildPage> {
        panic!("unexpected value child lookup")
    }
}

struct LaunchHarness {
    controller: Controller<RecordingTrace>,
    events: broadcast::Receiver<DebuggerEvent>,
    actions: Rc<RefCell<Vec<&'static str>>>,
    pid: Pid,
}

fn launch_controller() -> LaunchHarness {
    let actions = Rc::new(RefCell::new(Vec::new()));
    let pid = Pid::from_raw(4242);
    let trace = RecordingTrace {
        actions: Rc::clone(&actions),
        pid,
    };
    let image = Arc::new(ModuleImage::new(
        PathBuf::from("/test/program"),
        crate::TargetDescription {
            architecture: crate::Architecture::X86_64,
            byte_order: crate::ByteOrder::Little,
            pointer_width: crate::PointerWidth::Bits64,
        },
        AddressRange {
            start: ImageAddress::new(0),
            end: ImageAddress::new(0x1000),
        },
        crate::model::ModuleMetadata {
            functions: Vec::new(),
            code_instances: Vec::new(),
            symbols: Vec::new(),
            symbol_sources: crate::model::SymbolTableSources::default(),
            globals: Vec::new(),
            types: Arc::default(),
            source_files: Vec::new(),
            statements: Vec::new(),
            lines: Vec::new(),
            sections: Vec::new(),
        },
    ));
    let (controller, event_receiver) = test_controller(
        SessionLease::acquire().expect("acquire test session"),
        "/test/program",
        sectionless_elf(),
        image,
        trace,
        8,
    );

    LaunchHarness {
        controller,
        events: event_receiver,
        actions,
        pid,
    }
}

/// An ELF header without sections, so loader rendezvous discovery finds no
/// dynamic section to read through the fake.
fn sectionless_elf() -> Arc<[u8]> {
    let mut header = vec![0x7f, b'E', b'L', b'F', 2, 1, 1];
    header.resize(16, 0);
    // e_type through e_shstrndx: a little-endian x86-64 shared object with
    // no program or section headers.
    for (value, width) in [
        (3_u64, 2),
        (62, 2),
        (1, 4),
        (0, 8),
        (0, 8),
        (0, 8),
        (0, 4),
        (64, 2),
        (56, 2),
        (0, 2),
        (64, 2),
        (0, 2),
        (0, 2),
    ] {
        header.extend_from_slice(&value.to_le_bytes()[..width]);
    }
    header.into()
}

#[test]
fn controller_lifecycle_is_driven_through_the_linux_effect_boundary() {
    let LaunchHarness {
        mut controller,
        actions,
        pid,
        ..
    } = launch_controller();
    let (launch_reply, launch_result) = tokio::sync::oneshot::channel();

    controller.launch(launch_reply);
    controller
        .process_wait(WaitStatus::Stopped(pid, NixSignal::SIGTRAP))
        .expect("process initial stop");

    assert_eq!(
        launch_result
            .blocking_recv()
            .expect("launch reply")
            .expect("launch success"),
        ExecutionId::new(1)
    );

    let (shutdown_reply, shutdown_result) = tokio::sync::oneshot::channel();
    controller.begin_shutdown(Some(shutdown_reply));
    assert!(
        !controller.handle_shutdown_wait(WaitStatus::Signaled(pid, NixSignal::SIGKILL, false,))
    );
    shutdown_result
        .blocking_recv()
        .expect("shutdown reply")
        .expect("shutdown success");

    assert_eq!(
        actions.borrow().as_slice(),
        [
            "spawn",
            "spawn_waiter",
            "set_options",
            "load_bias",
            "continue",
            "kill",
        ]
    );
}

#[test]
fn pause_during_launch_completes_at_the_initial_exec_stop() {
    let LaunchHarness {
        mut controller,
        mut events,
        actions,
        pid,
    } = launch_controller();
    let (launch_reply, launch_result) = tokio::sync::oneshot::channel();
    controller.launch(launch_reply);

    assert_eq!(
        controller
            .begin_pause(process_id(pid))
            .expect("a launching inferior accepts a pause"),
        ExecutionId::new(1)
    );
    controller
        .process_wait(WaitStatus::Stopped(pid, NixSignal::SIGTRAP))
        .expect("process initial stop");

    assert_eq!(
        launch_result
            .blocking_recv()
            .expect("launch reply")
            .expect("launch success"),
        ExecutionId::new(1)
    );
    let published = std::iter::from_fn(|| events.try_recv().ok())
        .filter(|event| !matches!(event, DebuggerEvent::StateChanged { .. }))
        .collect::<Vec<_>>();
    assert!(
        matches!(
            published.as_slice(),
            [
                DebuggerEvent::InferiorLaunched { .. },
                DebuggerEvent::InferiorStopped {
                    execution_id: Some(execution),
                    reason: StopReason::Pause,
                    ..
                },
            ] if *execution == ExecutionId::new(1)
        ),
        "the paused launch must stop without first resuming: {published:?}"
    );
    assert_eq!(
        actions.borrow().as_slice(),
        [
            "spawn",
            "spawn_waiter",
            "set_options",
            "load_bias",
            "registers"
        ],
        "the initial thread must stop without a native stop request or resume"
    );
}

#[test]
fn virtual_inline_step_emits_a_new_stop_without_native_operations() {
    let VirtualStepHarness {
        mut controller,
        mut events,
        actions,
        pid,
    } = virtual_step_controller();

    let (execution, stopped) = controller
        .try_virtual_step(process_id(pid), StopId::new(1), pid, StepKind::IntoSource)
        .expect("virtual step")
        .expect("hidden child exists");

    assert_eq!(execution, ExecutionId::new(2));
    assert!(actions.borrow().is_empty());
    assert!(matches!(
        stopped,
        DebuggerEvent::InferiorStopped {
            execution_id: Some(execution_id),
            ..
        } if execution_id == ExecutionId::new(2)
    ));
    let emitted = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        !emitted
            .iter()
            .any(|event| matches!(event, DebuggerEvent::InferiorContinued { .. }))
    );
    let stop = controller
        .inferior
        .as_ref()
        .and_then(|inferior| inferior.public_stop.as_ref())
        .expect("new public stop");
    // Stop ids are allocated from a process-global counter, so the exact
    // value depends on concurrent test order. Assert only the invariant: the
    // virtual step minted a fresh id distinct from the launch stop (1).
    assert!(
        stop.id.get() > StopId::new(1).get(),
        "virtual step must mint a fresh stop id past the launch stop: {:?}",
        stop.id
    );
    assert_eq!(
        stop.presentations.get(&pid),
        Some(&FramePresentation {
            instruction: VirtualAddress::new(0x10),
            frame: PresentedFrame::Inline(CodeInstanceId::new(1)),
            hidden_inline_frames: 1,
        })
    );
    assert!(matches!(
        controller.try_virtual_step(process_id(pid), StopId::new(1), pid, StepKind::IntoSource,),
        Err(Error::StaleStop)
    ));
    assert!(actions.borrow().is_empty());
}

#[test]
fn outstanding_debugger_stop_is_reused_across_barriers() {
    let VirtualStepHarness {
        mut controller,
        actions,
        pid,
        ..
    } = virtual_step_controller();
    let thread = controller
        .inferior
        .as_mut()
        .and_then(|inferior| inferior.threads.get_mut(&pid))
        .expect("test thread exists");
    thread.state = NativeThreadState::Running;
    thread.debugger_stop_pending = true;

    controller
        .request_stops()
        .expect("reuse outstanding debugger stop");

    assert!(
        actions.borrow().is_empty(),
        "an outstanding SIGSTOP must not be duplicated"
    );
    let thread = controller
        .inferior
        .as_ref()
        .and_then(|inferior| inferior.threads.get(&pid))
        .expect("test thread exists");
    assert_eq!(thread.state, NativeThreadState::StopRequested);
    assert!(thread.debugger_stop_pending);
}

#[test]
fn user_breakpoint_supersedes_a_coincident_exception_barrier() {
    let VirtualStepHarness {
        mut controller,
        actions,
        pid,
        ..
    } = virtual_step_controller();
    let breakpoint_thread = Pid::from_raw(pid.as_raw() + 1);
    let pending_thread = Pid::from_raw(pid.as_raw() + 2);
    let exception = exception_info(NixSignal::SIGURG);
    let inferior = controller.inferior.as_mut().expect("test inferior exists");
    inferior.public_stop = None;
    inferior.thread_mut(pid).expect("test thread exists").reason =
        Some(StopReason::Exception(exception.clone()));
    inferior.threads.insert(
        breakpoint_thread,
        TraceThread {
            state: NativeThreadState::Running,
            expected: ExpectedStop::None,
            pending_signal: None,
            reason: None,
            stopped_at_breakpoint: None,
            awaiting_breakpoint: None,
            debugger_stop_pending: false,
            armed: None,
            watch_hits: BTreeSet::new(),
        },
    );
    inferior.threads.insert(
        pending_thread,
        TraceThread {
            state: NativeThreadState::StopRequested,
            expected: ExpectedStop::None,
            pending_signal: None,
            reason: None,
            stopped_at_breakpoint: None,
            awaiting_breakpoint: None,
            debugger_stop_pending: true,
            armed: None,
            watch_hits: BTreeSet::new(),
        },
    );
    inferior.barrier = Some(StopBarrier {
        execution: Some(ExecutionId::new(2)),
        triggering_thread: pid,
        reason: StopReason::Exception(exception),
    });
    let address = VirtualAddress::new(0x20);

    controller
        .begin_visible_stop(breakpoint_thread, StopReason::Breakpoint { address })
        .expect("record coincident breakpoint");

    let barrier = controller
        .inferior
        .as_ref()
        .and_then(|inferior| inferior.barrier.as_ref())
        .expect("pending thread keeps barrier active");
    assert_eq!(barrier.triggering_thread, breakpoint_thread);
    assert_eq!(barrier.reason, StopReason::Breakpoint { address });
    assert!(actions.borrow().is_empty());
}

#[test]
fn inline_range_start_presentation_obeys_the_step_kind() {
    let image = virtual_step_image();
    let InlineFrameLookup::Unique(chain) = image.locate(ImageAddress::new(0x10)).inline_frames
    else {
        panic!("test address has one inline chain");
    };

    assert_eq!(
        default_inline_visible_count(
            &image,
            chain.instances.as_ref(),
            ImageAddress::new(0x10),
            true,
        ),
        1,
        "the middle frame starts here while its nested child remains hidden"
    );
    assert_eq!(
        default_inline_visible_count(
            &image,
            chain.instances.as_ref(),
            ImageAddress::new(0x10),
            false,
        ),
        0,
        "next and finish retain the parent presentation"
    );
    assert_eq!(
        default_inline_visible_count(
            &image,
            chain.instances.as_ref(),
            ImageAddress::new(0x11),
            true,
        ),
        2,
        "away from a range boundary the innermost active frame is visible"
    );
}

fn virtual_step_image() -> Arc<ModuleImage> {
    let source = |line| SourceLocation {
        file: crate::SourceFileId::new(0),
        line: crate::LineNumber::new(line).expect("nonzero line"),
        column: None,
    };
    let range = Arc::from([AddressRange {
        start: ImageAddress::new(0x10),
        end: ImageAddress::new(0x20),
    }]);
    Arc::new(ModuleImage::new(
        PathBuf::from("/test/inline"),
        crate::TargetDescription {
            architecture: crate::Architecture::X86_64,
            byte_order: crate::ByteOrder::Little,
            pointer_width: crate::PointerWidth::Bits64,
        },
        AddressRange {
            start: ImageAddress::new(0),
            end: ImageAddress::new(0x100),
        },
        crate::model::ModuleMetadata {
            functions: ["physical", "middle", "leaf"]
                .into_iter()
                .enumerate()
                .map(|(id, name)| crate::FunctionInfo {
                    id: crate::FunctionId::new(u32::try_from(id).expect("small count")),
                    name: name.into(),
                    linkage_name: None,
                    declaration: None,
                })
                .collect(),
            code_instances: vec![
                crate::CodeInstanceInfo {
                    id: CodeInstanceId::new(0),
                    function: crate::FunctionId::new(0),
                    parent: None,
                    kind: CodeInstanceKind::OutOfLine,
                    ranges: Arc::from([AddressRange {
                        start: ImageAddress::new(0),
                        end: ImageAddress::new(0x100),
                    }]),
                    breakpoint_entry: None,
                },
                crate::CodeInstanceInfo {
                    id: CodeInstanceId::new(1),
                    function: crate::FunctionId::new(1),
                    parent: Some(CodeInstanceId::new(0)),
                    kind: CodeInstanceKind::Inline {
                        call_site: Some(source(10)),
                    },
                    ranges: Arc::clone(&range),
                    breakpoint_entry: None,
                },
                crate::CodeInstanceInfo {
                    id: CodeInstanceId::new(2),
                    function: crate::FunctionId::new(2),
                    parent: Some(CodeInstanceId::new(1)),
                    kind: CodeInstanceKind::Inline {
                        call_site: Some(source(20)),
                    },
                    ranges: range,
                    breakpoint_entry: None,
                },
            ],
            symbols: Vec::new(),
            symbol_sources: crate::model::SymbolTableSources::default(),
            globals: Vec::new(),
            types: Arc::default(),
            source_files: Vec::new(),
            statements: Vec::new(),
            lines: Vec::new(),
            sections: Vec::new(),
        },
    ))
}

struct VirtualStepHarness {
    controller: Controller<RecordingTrace>,
    events: broadcast::Receiver<DebuggerEvent>,
    actions: Rc<RefCell<Vec<&'static str>>>,
    pid: Pid,
}

fn virtual_step_controller() -> VirtualStepHarness {
    let pid = Pid::from_raw(4343);
    let actions = Rc::new(RefCell::new(Vec::new()));
    let trace = RecordingTrace {
        actions: Rc::clone(&actions),
        pid,
    };
    let image = virtual_step_image();
    let (mut controller, event_receiver) = test_controller(
        SessionLease::detached(),
        "/test/inline",
        Arc::from([]),
        Arc::clone(&image),
        trace,
        8,
    );
    controller.inferior = Some(virtual_step_inferior(pid, &image, StopId::new(1)));

    VirtualStepHarness {
        controller,
        events: event_receiver,
        actions,
        pid,
    }
}

fn virtual_step_inferior(pid: Pid, image: &ModuleImage, stop_id: StopId) -> Inferior {
    let presentation = FramePresentation {
        instruction: VirtualAddress::new(0x10),
        frame: PresentedFrame::Physical,
        hidden_inline_frames: 2,
    };
    Inferior {
        public_stop: Some(PublicStop {
            id: stop_id,
            triggering_thread: pid,
            reason: StopReason::Pause,
            presentations: BTreeMap::from([(pid, presentation)]),
        }),
        selected_thread: Some(pid),
        next_execution: 1,
        ..Inferior::new(
            InferiorOrigin::Launched,
            pid,
            LoadedModule::main(image.id(), 0),
            BTreeMap::from([(
                pid,
                TraceThread {
                    state: NativeThreadState::Stopped,
                    expected: ExpectedStop::None,
                    pending_signal: None,
                    reason: Some(StopReason::Pause),
                    stopped_at_breakpoint: None,
                    awaiting_breakpoint: None,
                    debugger_stop_pending: false,
                    armed: None,
                    watch_hits: BTreeSet::new(),
                },
            )]),
            None,
        )
    }
}

#[test]
fn frame_symbolization_adjusts_only_ordinary_caller_resume_addresses() {
    let stopped = FrameContext {
        instruction: VirtualAddress::new(0x1000),
        cfa: None,
        signal_frame: false,
    };
    let caller = FrameContext {
        instruction: VirtualAddress::new(0x2000),
        cfa: Some(VirtualAddress::new(0x3000)),
        signal_frame: false,
    };
    let signal = FrameContext {
        instruction: VirtualAddress::new(0x4000),
        cfa: Some(VirtualAddress::new(0x5000)),
        signal_frame: true,
    };

    assert_eq!(frame_lookup_address(0, &stopped), Some(stopped.instruction));
    assert_eq!(
        frame_lookup_address(1, &caller),
        Some(VirtualAddress::new(0x1fff))
    );
    assert_eq!(frame_lookup_address(2, &signal), Some(signal.instruction));
}

#[test]
fn thread_affinity_rejects_another_os_thread() {
    let affinity = ThreadAffinity::new();
    let result = thread::spawn(move || affinity.assert_owner()).join();

    assert!(result.is_err());
}

#[test]
fn queued_traps_are_read_from_the_threads_own_unblocked_pending_set() {
    let status = |pending: &str, blocked: &str| {
        format!(
            "Name:\tworker\nShdPnd:\t0000000000000010\nSigPnd:\t{pending}\nSigBlk:\t{blocked}\n"
        )
    };
    assert!(queued_trap_in_status(&status(
        "0000000000000010",
        "0000000000000000"
    )));
    assert!(queued_trap_in_status(&status(
        "0000000000010110",
        "0000000000000100"
    )));
    assert!(
        !queued_trap_in_status(&status("0000000000000010", "0000000000000010")),
        "a blocked trap cannot be dequeued by resuming"
    );
    assert!(
        !queued_trap_in_status(&status("0000000000000000", "0000000000000000")),
        "a process-wide trap is not the thread's own"
    );
    assert!(!queued_trap_in_status("SigPnd:\tnot-hex\n"));
    assert!(!queued_trap_in_status(""));
}

#[test]
fn raw_stop_format_preserves_siginfo_failure() {
    let record = RawStopRecord {
        status: "Stopped(7, SIGTRAP)".to_owned(),
        siginfo: Err(Errno::ESRCH),
    };

    assert_eq!(
        format_raw_stop(&record),
        "Stopped(7, SIGTRAP); PTRACE_GETSIGINFO failed: ESRCH: No such process"
    );
}

#[test]
fn stop_classifier_preserves_signal_and_trap_provenance() {
    let metadata = |code| {
        Ok(SignalMetadata {
            code,
            sender: (code <= 0).then_some(71),
        })
    };
    let classify = |signal, siginfo, expected, breakpoint| {
        classify_stop_evidence(
            signal,
            "raw-status".to_owned(),
            siginfo,
            expected,
            false,
            false,
            breakpoint,
            WatchStatus::Absent,
        )
    };

    assert!(matches!(
        classify(
            NixSignal::SIGTRAP,
            metadata(libc::TRAP_TRACE),
            &ExpectedStop::UserStep {
                kind: StepKind::Instruction
            },
            None,
        ),
        ClassifiedStop::Trace { ref watch } if watch.is_empty()
    ));
    assert!(matches!(
        classify(
            NixSignal::SIGTRAP,
            metadata(libc::SI_TKILL),
            &ExpectedStop::None,
            None,
        ),
        ClassifiedStop::SignalDelivery(PendingSignal {
            signal: NixSignal::SIGTRAP,
            sender: Some(71),
            ..
        })
    ));
    assert!(matches!(
        classify(
            NixSignal::SIGSEGV,
            metadata(libc::SI_KERNEL),
            &ExpectedStop::None,
            None,
        ),
        ClassifiedStop::SignalDelivery(PendingSignal {
            signal: NixSignal::SIGSEGV,
            code: libc::SI_KERNEL,
            ..
        })
    ));
    assert!(matches!(
        classify(
            NixSignal::SIGSTOP,
            Err(Errno::EINVAL),
            &ExpectedStop::None,
            None,
        ),
        ClassifiedStop::GroupStop(NixSignal::SIGSTOP)
    ));
    assert!(matches!(
        classify(
            NixSignal::SIGTRAP,
            Err(Errno::ESRCH),
            &ExpectedStop::None,
            None,
        ),
        ClassifiedStop::Unclassifiable(RawStopRecord {
            siginfo: Err(Errno::ESRCH),
            ..
        })
    ));
    assert!(matches!(
        classify(
            NixSignal::SIGTRAP,
            metadata(libc::SI_KERNEL),
            &ExpectedStop::None,
            Some(VirtualAddress::new(0x1234)),
        ),
        ClassifiedStop::Breakpoint(address) if address == VirtualAddress::new(0x1234)
    ));
}

/// Models per-thread debug registers the way Linux exposes them, records
/// every native effect, and injects failures.
#[derive(Default)]
struct DebugRegisterTrace {
    actions: RefCell<Vec<String>>,
    registers: RefCell<BTreeMap<Pid, [u64; 8]>>,
    /// Fails a write of `(thread, register)` after skipping that many
    /// successful writes.
    failures: RefCell<BTreeMap<(Pid, usize), (u32, Errno)>>,
    /// Accepts writes without storing them, like gVisor.
    discard_writes: bool,
    siginfo: RefCell<BTreeMap<Pid, SignalMetadata>>,
    program_counters: RefCell<BTreeMap<Pid, u64>>,
    /// Threads holding a SIGTRAP queued behind an interrupt stop.
    queued_traps: RefCell<BTreeSet<Pid>>,
    /// Threads a sibling's `exit_group` killed out of their ptrace-stop.
    vanished: RefCell<BTreeSet<Pid>>,
    /// The child reported by the next clone event, in the process `tgid`.
    clone: RefCell<Option<(Pid, Pid)>>,
}

impl DebugRegisterTrace {
    fn record(&self, action: String) {
        self.actions.borrow_mut().push(action);
    }

    fn take_actions(&self) -> Vec<String> {
        std::mem::take(&mut *self.actions.borrow_mut())
    }

    fn registers_of(&self, pid: Pid) -> [u64; 8] {
        self.registers.borrow().get(&pid).copied().unwrap_or([
            0,
            0,
            0,
            0,
            0,
            0,
            debug_registers::STATUS_IDLE,
            0,
        ])
    }

    fn fail_after(&self, pid: Pid, register: usize, skips: u32, error: Errno) {
        self.failures
            .borrow_mut()
            .insert((pid, register), (skips, error));
    }
}

impl InspectionOps for DebugRegisterTrace {
    fn read_word(&self, _pid: Pid, _address: u64) -> Result<u64> {
        Ok(0)
    }

    fn registers(&self, pid: Pid) -> Result<libc::user_regs_struct> {
        let mut registers = RecordingTrace {
            actions: Rc::new(RefCell::new(Vec::new())),
            pid,
        }
        .registers(pid)?;
        if let Some(&rip) = self.program_counters.borrow().get(&pid) {
            registers.rip = rip;
        }
        Ok(registers)
    }
}

impl LinuxTraceOps for DebugRegisterTrace {
    fn spawn(&self, _executable: &Path) -> Result<Pid> {
        RecordingTrace::unexpected("spawn")
    }
    fn spawn_waiter(&self, _messages: mpsc::Sender<ControllerMessage>) -> Result<Waiter> {
        Ok(Waiter {
            stop: Arc::new(AtomicBool::new(false)),
            thread: thread::spawn(|| {}),
        })
    }
    fn process_threads(&self, _process: Pid) -> Result<Vec<Pid>> {
        RecordingTrace::unexpected("process_threads")
    }
    fn seize(&self, _pid: Pid) -> Result<bool> {
        RecordingTrace::unexpected("seize")
    }
    fn interrupt(&self, pid: Pid) -> Result<bool> {
        self.record(format!("interrupt {pid}"));
        Ok(true)
    }
    fn detach(&self, pid: Pid, signal: Option<NixSignal>) -> Result<()> {
        self.record(format!("detach {pid} {signal:?}"));
        Ok(())
    }
    fn kill(&self, pid: Pid, signal: NixSignal) -> Result<()> {
        self.record(format!("kill {pid} {signal}"));
        Ok(())
    }
    fn reap(&self, _pid: Pid) -> Result<()> {
        RecordingTrace::unexpected("reap")
    }
    fn thread_group_id(&self, _pid: Pid) -> Result<Pid> {
        Ok(self.clone.borrow().expect("a clone is pending").1)
    }
    fn load_bias(
        &self,
        _pid: Pid,
        _executable: &Path,
        _executable_data: &[u8],
        _identity: FileIdentity,
    ) -> Result<u64> {
        RecordingTrace::unexpected("load_bias")
    }
    fn write_word(&self, _pid: Pid, _address: u64, _value: u64) -> Result<()> {
        RecordingTrace::unexpected("write_word")
    }
    fn continue_execution(&self, pid: Pid, signal: Option<NixSignal>) -> Result<()> {
        self.record(format!("continue {pid} {signal:?}"));
        if self.vanished.borrow().contains(&pid) {
            return Err(backend_error(LinuxError::System(Errno::ESRCH)));
        }
        Ok(())
    }
    fn continue_during_shutdown(&self, _pid: Pid) -> Result<()> {
        RecordingTrace::unexpected("continue_during_shutdown")
    }
    fn step(&self, pid: Pid, _signal: Option<NixSignal>) -> Result<()> {
        self.record(format!("step {pid}"));
        Ok(())
    }
    fn set_registers(&self, pid: Pid, registers: libc::user_regs_struct) -> Result<()> {
        self.record(format!("set_registers {pid} rip={:#x}", registers.rip));
        Ok(())
    }
    fn set_options(&self, pid: Pid, _exit_kill: bool) -> Result<()> {
        self.record(format!("set_options {pid}"));
        Ok(())
    }
    fn event_message(&self, _pid: Pid) -> Result<libc::c_long> {
        Ok(libc::c_long::from(
            self.clone.borrow().expect("a clone is pending").0.as_raw(),
        ))
    }
    fn signal_metadata(&self, pid: Pid) -> std::result::Result<SignalMetadata, Errno> {
        self.siginfo
            .borrow()
            .get(&pid)
            .copied()
            .ok_or(Errno::EINVAL)
    }
    fn request_stop(&self, _process: Pid, thread: Pid) -> Result<()> {
        self.record(format!("request_stop {thread}"));
        Ok(())
    }
    fn queued_trap(&self, pid: Pid) -> Result<bool> {
        Ok(self.queued_traps.borrow().contains(&pid))
    }
    fn read_debug_register(&self, pid: Pid, index: usize) -> std::result::Result<u64, Errno> {
        self.record(format!("read {pid} dr{index}"));
        Ok(self.registers_of(pid)[index])
    }
    fn write_debug_register(
        &self,
        pid: Pid,
        index: usize,
        value: u64,
    ) -> std::result::Result<(), Errno> {
        self.record(format!("write {pid} dr{index}={value:#x}"));
        let mut failures = self.failures.borrow_mut();
        if let Some((skips, error)) = failures.get_mut(&(pid, index)) {
            if *skips == 0 {
                let error = *error;
                failures.remove(&(pid, index));
                return Err(error);
            }
            *skips -= 1;
        }
        drop(failures);
        if !self.discard_writes {
            let mut registers = self.registers_of(pid);
            registers[index] = value;
            self.registers.borrow_mut().insert(pid, registers);
        }
        Ok(())
    }
    fn install_breakpoint(
        &self,
        _pid: Pid,
        _sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        _address: VirtualAddress,
        _owner: BreakpointOwner,
    ) -> Result<()> {
        RecordingTrace::unexpected("install_breakpoint")
    }
    fn remove_breakpoint(
        &self,
        _pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
    ) -> Result<()> {
        self.record(format!("remove_site {address}"));
        sites.get_mut(&address).expect("known site").installed = false;
        Ok(())
    }
    fn reinstall_breakpoint(
        &self,
        _pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
    ) -> Result<()> {
        self.record(format!("reinstall_site {address}"));
        sites.get_mut(&address).expect("known site").installed = true;
        Ok(())
    }
}

struct WatchHarness {
    controller: Controller<DebugRegisterTrace>,
    events: broadcast::Receiver<DebuggerEvent>,
    threads: Vec<Pid>,
}

impl WatchHarness {
    fn trace(&self) -> &DebugRegisterTrace {
        &self.controller.ptrace
    }

    fn add(&mut self, address: u64, byte_size: u64) -> Result<Watchpoint> {
        self.controller.add_watchpoint(
            WatchpointSpec::Location {
                address: VirtualAddress::new(address),
                byte_size,
            },
            WatchAccess::Write,
        )
    }

    fn thread(&mut self, pid: Pid) -> &mut TraceThread {
        self.controller
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.threads.get_mut(&pid))
            .expect("harness thread exists")
    }

    fn watch_events(&mut self) -> usize {
        std::iter::from_fn(|| self.events.try_recv().ok())
            .filter(|event| matches!(event, DebuggerEvent::WatchpointsChanged { .. }))
            .count()
    }

    /// Marks every thread running inside a process-wide continue.
    fn start_continue(&mut self) {
        let inferior = self.controller.inferior.as_mut().expect("inferior");
        inferior.public_stop = None;
        inferior.active = Some(ActiveExecution {
            id: ExecutionId::new(2),
            kind: ActiveKind::Continue,
            scope: ResumeScope::Process(process_id(inferior.tgid)),
            resume_threads: inferior.threads.keys().copied().collect(),
        });
        for thread in inferior.threads.values_mut() {
            thread.state = NativeThreadState::Running;
            thread.reason = None;
        }
    }

    /// Delivers the SIGSTOP each still-running thread was asked for. A
    /// thread that stopped for another reason keeps its SIGSTOP pending.
    fn settle_requested_stops(&mut self) {
        let requested = self
            .controller
            .inferior
            .as_ref()
            .expect("inferior")
            .threads
            .iter()
            .filter(|(_, thread)| {
                thread.debugger_stop_pending
                    && matches!(thread.state, NativeThreadState::StopRequested)
            })
            .map(|(&pid, _)| pid)
            .collect::<Vec<_>>();
        for pid in requested {
            self.trace().siginfo.borrow_mut().insert(
                pid,
                SignalMetadata {
                    code: libc::SI_TKILL,
                    sender: Some(i32::try_from(std::process::id()).expect("pid fits")),
                },
            );
            self.controller
                .process_wait(WaitStatus::Stopped(pid, NixSignal::SIGSTOP))
                .expect("debugger stop");
        }
    }

    /// Reports a SIGTRAP with `code` while DR6 holds `status`.
    fn trap(&mut self, pid: Pid, code: i32, status: u64) -> Result<()> {
        let mut registers = self.trace().registers_of(pid);
        registers[debug_registers::STATUS_REGISTER] = status;
        self.trace().registers.borrow_mut().insert(pid, registers);
        self.trace()
            .siginfo
            .borrow_mut()
            .insert(pid, SignalMetadata { code, sender: None });
        self.controller
            .process_wait(WaitStatus::Stopped(pid, NixSignal::SIGTRAP))
    }

    fn public_reason(&self) -> Option<StopReason> {
        self.controller
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.public_stop.as_ref())
            .map(|stop| stop.reason.clone())
    }
}

fn watch_harness(thread_count: i32) -> WatchHarness {
    let threads = (0..thread_count)
        .map(|offset| Pid::from_raw(5000 + offset))
        .collect::<Vec<_>>();
    let image = virtual_step_image();
    let (mut controller, event_receiver) = test_controller(
        SessionLease::detached(),
        "/test/watch",
        sectionless_elf(),
        Arc::clone(&image),
        DebugRegisterTrace::default(),
        256,
    );
    let mut inferior = virtual_step_inferior(threads[0], &image, StopId::new(1));
    for &pid in &threads[1..] {
        inferior
            .threads
            .insert(pid, TraceThread::starting(ExpectedStop::None));
    }
    for thread in inferior.threads.values_mut() {
        thread.state = NativeThreadState::Stopped;
        thread.armed = Some(0);
    }
    controller.inferior = Some(inferior);
    WatchHarness {
        controller,
        events: event_receiver,
        threads,
    }
}

#[test]
fn arming_programs_each_thread_disabled_first_and_verifies_it() {
    let mut harness = watch_harness(3);
    let watchpoint = harness.add(0x6006, 4).expect("arm watchpoint");
    assert_eq!(
        watchpoint.coverage.as_ref(),
        [
            AddressRange {
                start: VirtualAddress::new(0x6006),
                end: VirtualAddress::new(0x6008),
            },
            AddressRange {
                start: VirtualAddress::new(0x6008),
                end: VirtualAddress::new(0x600a),
            },
        ]
    );
    let control = 0x0055_0005;
    let expected = harness
        .threads
        .iter()
        .flat_map(|pid| {
            [
                format!("write {pid} dr7=0x0"),
                format!("write {pid} dr0=0x6006"),
                format!("write {pid} dr1=0x6008"),
                format!("write {pid} dr7={control:#x}"),
                format!("read {pid} dr7"),
                format!("read {pid} dr0"),
                format!("read {pid} dr1"),
            ]
        })
        .collect::<Vec<_>>();
    assert_eq!(harness.trace().take_actions(), expected);
    assert_eq!(harness.watch_events(), 1);
    for pid in harness.threads.clone() {
        assert_eq!(harness.thread(pid).armed, Some(1));
        assert_eq!(harness.trace().registers_of(pid)[7], control);
    }
}

#[test]
fn a_failure_on_any_thread_restores_every_thread_and_publishes_nothing() {
    for failing_thread in 0..3 {
        let mut harness = watch_harness(3);
        let first = harness.add(0x7000, 8).expect("arm first watchpoint");
        let armed = harness
            .threads
            .iter()
            .map(|&pid| harness.trace().registers_of(pid))
            .collect::<Vec<_>>();
        harness.watch_events();
        let failing = harness.threads[failing_thread];
        harness.trace().fail_after(failing, 1, 0, Errno::ENOSPC);

        let result = harness.add(0x8000, 8);
        assert!(
            matches!(
                result,
                Err(Error::WatchpointHardwareBusy { thread }) if thread == debug_thread_id(failing)
            ),
            "{result:?}"
        );
        for (index, &pid) in harness.threads.clone().iter().enumerate() {
            assert_eq!(
                harness.trace().registers_of(pid)[7],
                armed[index][7],
                "thread {index} keeps only the first watchpoint"
            );
            assert_eq!(harness.thread(pid).armed, Some(1));
        }
        assert_eq!(harness.watch_events(), 0);
        let inferior = harness.controller.inferior.as_ref().expect("inferior");
        assert_eq!(
            inferior
                .watch
                .watchpoints
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            [first.id]
        );
        assert_eq!(inferior.watch.generation, 1);
    }
}

#[test]
fn a_failed_rollback_kills_the_inferior_and_reports_both_failures() {
    let mut harness = watch_harness(2);
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };
    // The second thread refuses its address; restoring the first thread
    // then fails on its third control write, the rollback's disable.
    harness.trace().fail_after(second, 0, 0, Errno::ENOSPC);
    harness.trace().fail_after(first, 7, 2, Errno::EPERM);

    let result = harness.add(0x9000, 8);
    assert!(
        matches!(
            &result,
            Err(Error::Backend(error)) if matches!(
                error.downcast_ref::<LinuxError>(),
                Some(LinuxError::WatchpointArmRecovery { .. })
            )
        ),
        "{result:?}"
    );
    assert!(
        harness
            .trace()
            .take_actions()
            .contains(&format!("kill {first} SIGKILL")),
        "an inferior with unknown debug registers is not left running"
    );
    assert_eq!(harness.watch_events(), 0);
}

#[test]
fn readback_mismatches_report_unavailable_hardware_and_arm_nothing() {
    let mut harness = watch_harness(2);
    harness.controller.ptrace.discard_writes = true;
    let result = harness.add(0xa000, 8);
    assert!(
        matches!(result, Err(Error::HardwareWatchpointsUnavailable(_))),
        "{result:?}"
    );
    assert_eq!(harness.watch_events(), 0);
    assert!(
        harness
            .controller
            .inferior
            .as_ref()
            .expect("inferior")
            .watch
            .watchpoints
            .is_empty()
    );
}

#[test]
fn an_exiting_thread_is_skipped_and_armed_when_execution_resumes() {
    let mut harness = watch_harness(3);
    let exiting = harness.threads[1];
    harness.trace().fail_after(exiting, 7, 0, Errno::ESRCH);
    harness
        .add(0xb000, 8)
        .expect("arm despite an exiting thread");
    assert_eq!(harness.thread(exiting).armed, Some(0));
    assert_eq!(harness.thread(harness.threads[0]).armed, Some(1));
    assert_eq!(harness.thread(harness.threads[2]).armed, Some(1));

    // A thread that is still present is programmed before anything runs.
    harness.trace().take_actions();
    harness
        .controller
        .sync_debug_registers()
        .expect("sync stale thread");
    let actions = harness.trace().take_actions();
    assert!(
        actions
            .iter()
            .all(|action| action.contains(&exiting.to_string()))
    );
    assert_eq!(harness.thread(exiting).armed, Some(1));
}

#[test]
fn new_threads_are_armed_before_they_first_run() {
    let mut harness = watch_harness(1);
    harness.add(0xc000, 8).expect("arm");
    harness.start_continue();
    let child = Pid::from_raw(6000);
    {
        let inferior = harness.controller.inferior.as_mut().expect("inferior");
        inferior
            .threads
            .insert(child, TraceThread::starting(ExpectedStop::None));
        inferior
            .active
            .as_mut()
            .expect("continue")
            .resume_threads
            .insert(child);
    }
    harness.trace().take_actions();
    harness
        .controller
        .process_wait(WaitStatus::Stopped(child, NixSignal::SIGSTOP))
        .expect("thread start");
    let actions = harness.trace().take_actions();
    let armed = actions
        .iter()
        .position(|action| action == &format!("write {child} dr7=0x90001"))
        .expect("the child was armed");
    let resumed = actions
        .iter()
        .position(|action| action == &format!("continue {child} None"))
        .expect("the child was resumed");
    assert!(armed < resumed, "{actions:?}");
    assert_eq!(harness.thread(child).armed, Some(1));
}

#[test]
fn a_new_thread_that_cannot_be_armed_never_runs() {
    let mut harness = watch_harness(1);
    harness.add(0xc000, 8).expect("arm");
    harness.start_continue();
    let child = Pid::from_raw(6000);
    harness
        .controller
        .inferior
        .as_mut()
        .expect("inferior")
        .threads
        .insert(child, TraceThread::starting(ExpectedStop::None));
    harness.trace().fail_after(child, 0, 0, Errno::ENOSPC);
    harness.trace().take_actions();
    harness
        .controller
        .process_wait(WaitStatus::Stopped(child, NixSignal::SIGSTOP))
        .expect("thread start");
    harness.settle_requested_stops();

    let reason = harness.public_reason().expect("a public stop");
    assert!(
        matches!(
            &reason,
            StopReason::WatchpointArmFailed { thread_id, .. } if *thread_id == debug_thread_id(child)
        ),
        "{reason:?}"
    );
    let actions = harness.trace().take_actions();
    assert!(
        !actions
            .iter()
            .any(|action| action.starts_with(&format!("continue {child}"))),
        "{actions:?}"
    );

    // Resuming retries the arming and refuses to run while it fails.
    harness.trace().fail_after(child, 0, 0, Errno::ENOSPC);
    let stop = harness
        .controller
        .inferior
        .as_ref()
        .and_then(|inferior| inferior.public_stop.as_ref())
        .expect("stop")
        .id;
    let tgid = harness.threads[0];
    let result = harness.controller.begin_execution(
        process_id(tgid),
        stop,
        ResumeScope::Process(process_id(tgid)),
        ActiveKind::Continue,
        ExceptionDisposition::Pass,
    );
    assert!(
        matches!(result, Err(Error::WatchpointHardwareBusy { thread }) if thread == debug_thread_id(child)),
        "{result:?}"
    );
    assert!(
        !harness
            .trace()
            .take_actions()
            .iter()
            .any(|action| action.starts_with("continue")),
    );
}

#[test]
fn hits_are_attributed_from_dr6_without_rewinding_the_pc() {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    let first = harness.add(0xd000, 8).expect("first");
    let second = harness.add(0xd008, 8).expect("second");
    // An installed site just before the reported PC must not turn the
    // hardware trap into an int3.
    harness
        .trace()
        .program_counters
        .borrow_mut()
        .insert(pid, 0x41);
    harness
        .controller
        .inferior
        .as_mut()
        .expect("inferior")
        .breakpoints
        .insert(
            VirtualAddress::new(0x40),
            BreakpointSite {
                original_byte: 0x90,
                installed: true,
                owners: BTreeSet::from([BreakpointOwner::User(BreakpointId::new(1))]),
            },
        );
    harness.start_continue();
    harness.trace().take_actions();
    harness
        .trap(
            pid,
            TRAP_HARDWARE_BREAKPOINT,
            debug_registers::STATUS_IDLE | 0b11,
        )
        .expect("watch trap");

    let StopReason::Watchpoint { hits } = harness.public_reason().expect("stop") else {
        panic!("expected a watchpoint stop");
    };
    assert_eq!(
        hits.iter().map(|hit| hit.watchpoint).collect::<Vec<_>>(),
        [first.id, second.id]
    );
    let actions = harness.trace().take_actions();
    assert!(
        !actions
            .iter()
            .any(|action| action.starts_with("set_registers"))
    );
    let read = actions
        .iter()
        .position(|action| action == &format!("read {pid} dr6"))
        .expect("DR6 was read");
    assert_eq!(
        actions[read + 1],
        format!("write {pid} dr6={:#x}", debug_registers::STATUS_IDLE),
        "DR6 is cleared once consumed"
    );
}

#[test]
fn stale_status_is_never_consulted_outside_debug_exceptions() {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    harness.add(0xd000, 8).expect("arm");
    harness
        .trace()
        .program_counters
        .borrow_mut()
        .insert(pid, 0x41);
    harness
        .controller
        .inferior
        .as_mut()
        .expect("inferior")
        .breakpoints
        .insert(
            VirtualAddress::new(0x40),
            BreakpointSite {
                original_byte: 0x90,
                installed: true,
                owners: BTreeSet::from([BreakpointOwner::User(BreakpointId::new(1))]),
            },
        );
    harness.start_continue();
    harness.trace().take_actions();
    // An int3 stop while DR6 still records an earlier hit.
    harness
        .trap(pid, libc::SI_KERNEL, debug_registers::STATUS_IDLE | 0b1)
        .expect("int3");
    assert_eq!(
        harness.public_reason(),
        Some(StopReason::Breakpoint {
            address: VirtualAddress::new(0x40)
        })
    );
    assert!(
        !harness
            .trace()
            .take_actions()
            .iter()
            .any(|action| action.contains("dr6")),
        "DR6 is not read at an int3 stop"
    );
}

#[test]
fn a_hit_in_an_unowned_slot_is_unclassifiable() {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    harness.add(0xd000, 8).expect("arm");
    harness.start_continue();
    harness
        .trap(
            pid,
            TRAP_HARDWARE_BREAKPOINT,
            debug_registers::STATUS_IDLE | 0b100,
        )
        .expect("foreign trap");
    assert!(matches!(
        harness.public_reason(),
        Some(StopReason::Unclassifiable { description }) if description.contains("no watchpoint owns")
    ));

    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    harness.start_continue();
    harness
        .trap(pid, TRAP_HARDWARE_BREAKPOINT, debug_registers::STATUS_IDLE)
        .expect("trap without a slot");
    assert!(matches!(
        harness.public_reason(),
        Some(StopReason::Unclassifiable { .. })
    ));
}

#[test]
fn a_hit_while_stepping_over_a_breakpoint_completes_the_repair_and_stops() {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    let watchpoint = harness.add(0xe000, 8).expect("arm");
    let site = VirtualAddress::new(0x40);
    {
        let inferior = harness.controller.inferior.as_mut().expect("inferior");
        inferior.breakpoints.insert(
            site,
            BreakpointSite {
                original_byte: 0x90,
                installed: false,
                owners: BTreeSet::from([BreakpointOwner::User(BreakpointId::new(1))]),
            },
        );
        inferior.repairs = VecDeque::from([RepairGroup {
            address: site,
            remaining: VecDeque::new(),
            current: Some(pid),
            site_removed: true,
        }]);
    }
    harness.start_continue();
    harness.thread(pid).expected = ExpectedStop::BreakpointRepair { address: site };
    harness.trace().take_actions();
    harness
        .trap(
            pid,
            libc::TRAP_TRACE,
            debug_registers::STATUS_IDLE | 1 << 14 | 0b1,
        )
        .expect("watched repair step");

    let StopReason::Watchpoint { hits } = harness.public_reason().expect("stop") else {
        panic!("expected the repair step's hit");
    };
    assert_eq!(hits[0].watchpoint, watchpoint.id);
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert!(inferior.repairs.is_empty());
    assert!(
        inferior.breakpoints[&site].installed,
        "the site is restored"
    );
    assert_eq!(inferior.threads[&pid].stopped_at_breakpoint, None);
}

#[test]
fn detaching_disarms_every_thread_first_and_never_redelivers_a_watch_trap() {
    let mut harness = watch_harness(2);
    harness.add(0xf000, 8).expect("arm");
    {
        let inferior = harness.controller.inferior.as_mut().expect("inferior");
        inferior.origin = InferiorOrigin::Attached;
    }
    harness.start_continue();
    // One thread reports a watch trap while the detach interrupts it.
    let (reply, result) = tokio::sync::oneshot::channel();
    harness.controller.begin_shutdown(Some(reply));
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };
    let mut registers = harness.trace().registers_of(first);
    registers[debug_registers::STATUS_REGISTER] = debug_registers::STATUS_IDLE | 0b1;
    harness
        .trace()
        .registers
        .borrow_mut()
        .insert(first, registers);
    harness.trace().siginfo.borrow_mut().insert(
        first,
        SignalMetadata {
            code: TRAP_HARDWARE_BREAKPOINT,
            sender: None,
        },
    );
    assert!(
        harness
            .controller
            .handle_detach_wait(WaitStatus::Stopped(first, NixSignal::SIGTRAP))
    );
    harness.trace().take_actions();
    assert!(
        !harness
            .controller
            .handle_detach_wait(WaitStatus::PtraceEvent(
                second,
                NixSignal::SIGTRAP,
                libc::PTRACE_EVENT_STOP
            ))
    );
    result
        .blocking_recv()
        .expect("shutdown reply")
        .expect("detached");

    let actions = harness.trace().take_actions();
    let first_detach = actions
        .iter()
        .position(|action| action.starts_with("detach"))
        .expect("threads were detached");
    for pid in [first, second] {
        let disarm = actions
            .iter()
            .position(|action| action == &format!("write {pid} dr7=0x0"))
            .expect("every thread is disarmed");
        assert!(disarm < first_detach, "{actions:?}");
        assert!(
            actions.contains(&format!("detach {pid} None")),
            "no signal is delivered on detach: {actions:?}"
        );
    }
}

#[test]
fn a_failed_disarm_still_detaches_and_publishes_the_detach() {
    let mut harness = watch_harness(2);
    harness.add(0xf000, 8).expect("arm");
    harness
        .controller
        .inferior
        .as_mut()
        .expect("inferior")
        .origin = InferiorOrigin::Attached;
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };
    harness.trace().fail_after(first, 7, 0, Errno::EPERM);
    harness.trace().take_actions();
    while harness.events.try_recv().is_ok() {}

    let (reply, result) = tokio::sync::oneshot::channel();
    harness.controller.begin_shutdown(Some(reply));
    let result = result.blocking_recv().expect("shutdown reply");
    assert!(result.is_err(), "the disarm failure is reported");

    // Keeping the process traced would not help: the tracer is exiting.
    let actions = harness.trace().take_actions();
    for pid in [first, second] {
        assert!(
            actions.contains(&format!("detach {pid} None")),
            "{actions:?}"
        );
    }
    assert!(harness.controller.inferior.is_none());
    let detached = std::iter::from_fn(|| harness.events.try_recv().ok())
        .filter(|event| matches!(event, DebuggerEvent::InferiorDetached { .. }))
        .count();
    assert_eq!(detached, 1, "clients learn the process is gone");
}

#[test]
fn an_attached_stop_collects_traps_queued_behind_its_interrupt() {
    let mut harness = watch_harness(2);
    let watchpoint = harness.add(0x1_0000, 8).expect("arm");
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };
    harness
        .controller
        .inferior
        .as_mut()
        .expect("inferior")
        .origin = InferiorOrigin::Attached;
    harness.start_continue();
    harness.trace().queued_traps.borrow_mut().insert(second);
    harness.trace().take_actions();

    // The first thread hits; the second is interrupted with its own hit
    // still queued.
    harness
        .trap(
            first,
            TRAP_HARDWARE_BREAKPOINT,
            debug_registers::STATUS_IDLE | 1,
        )
        .expect("first hit");
    assert!(
        harness
            .trace()
            .take_actions()
            .contains(&format!("interrupt {second}"))
    );
    harness
        .controller
        .process_wait(WaitStatus::PtraceEvent(
            second,
            NixSignal::SIGTRAP,
            libc::PTRACE_EVENT_STOP,
        ))
        .expect("interrupt stop");
    assert_eq!(
        harness.public_reason(),
        None,
        "the queued trap is collected first"
    );
    assert_eq!(
        harness.trace().take_actions(),
        [format!("continue {second} None")]
    );

    harness.trace().queued_traps.borrow_mut().clear();
    harness
        .trap(
            second,
            TRAP_HARDWARE_BREAKPOINT,
            debug_registers::STATUS_IDLE | 1,
        )
        .expect("queued hit");
    assert!(matches!(
        harness.public_reason(),
        Some(StopReason::Watchpoint { ref hits }) if hits[0].watchpoint == watchpoint.id
    ));
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    for pid in [first, second] {
        assert!(matches!(
            inferior.threads[&pid].reason,
            Some(StopReason::Watchpoint { ref hits }) if hits[0].thread == debug_thread_id(pid)
        ));
    }
}

#[test]
fn detaching_collects_queued_traps_before_releasing_the_process() {
    let mut harness = watch_harness(1);
    harness.add(0x1_0000, 8).expect("arm");
    let pid = harness.threads[0];
    harness
        .controller
        .inferior
        .as_mut()
        .expect("inferior")
        .origin = InferiorOrigin::Attached;
    harness.trace().queued_traps.borrow_mut().insert(pid);
    harness.trace().take_actions();

    let (reply, result) = tokio::sync::oneshot::channel();
    harness.controller.begin_shutdown(Some(reply));
    assert_eq!(
        harness.trace().take_actions(),
        [format!("continue {pid} None")],
        "detaching waits for the queued trap"
    );

    harness.trace().queued_traps.borrow_mut().clear();
    let mut registers = harness.trace().registers_of(pid);
    registers[debug_registers::STATUS_REGISTER] = debug_registers::STATUS_IDLE | 1;
    harness
        .trace()
        .registers
        .borrow_mut()
        .insert(pid, registers);
    harness.trace().siginfo.borrow_mut().insert(
        pid,
        SignalMetadata {
            code: TRAP_HARDWARE_BREAKPOINT,
            sender: None,
        },
    );
    assert!(
        !harness
            .controller
            .handle_detach_wait(WaitStatus::Stopped(pid, NixSignal::SIGTRAP))
    );
    result
        .blocking_recv()
        .expect("shutdown reply")
        .expect("detached");
    let actions = harness.trace().take_actions();
    assert!(
        actions.contains(&format!("detach {pid} None")),
        "the collected trap is not delivered: {actions:?}"
    );
}

#[test]
fn continuing_tolerates_threads_killed_by_a_siblings_exit() {
    let mut harness = watch_harness(3);
    let [first, second, third] = harness.threads[..] else {
        unreachable!("three threads");
    };
    // The first thread runs exit_group as soon as it resumes, killing
    // its siblings out of their stops.
    harness
        .trace()
        .vanished
        .borrow_mut()
        .extend([second, third]);
    let stop = harness
        .controller
        .inferior
        .as_ref()
        .and_then(|inferior| inferior.public_stop.as_ref())
        .expect("stop")
        .id;
    harness
        .controller
        .begin_execution(
            process_id(first),
            stop,
            ResumeScope::Process(process_id(first)),
            ActiveKind::Continue,
            ExceptionDisposition::Pass,
        )
        .expect("the continue succeeds");
    assert_eq!(harness.thread(first).state, NativeThreadState::Running);
    for pid in [second, third] {
        assert_eq!(harness.thread(pid).state, NativeThreadState::Exiting);
    }

    // Their exits retire them, and the process exit ends the session.
    for pid in [second, third, first] {
        harness
            .controller
            .process_wait(WaitStatus::Exited(pid, 0))
            .expect("exit");
    }
    assert!(harness.controller.inferior.is_none());
}

#[test]
fn a_hit_during_a_pause_outranks_the_pause() {
    let mut harness = watch_harness(2);
    let watchpoint = harness.add(0x1_1000, 8).expect("arm");
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };
    harness.start_continue();
    harness
        .controller
        .begin_pause(process_id(first))
        .expect("pause");
    // The first thread hits before its requested stop is delivered.
    harness
        .trap(
            first,
            TRAP_HARDWARE_BREAKPOINT,
            debug_registers::STATUS_IDLE | 1,
        )
        .expect("hit during pause");
    assert_eq!(
        harness.public_reason(),
        None,
        "the second thread is running"
    );
    harness.settle_requested_stops();
    let Some(StopReason::Watchpoint { hits }) = harness.public_reason() else {
        panic!("the hit outranks the pause: {:?}", harness.public_reason());
    };
    assert_eq!(hits[0].watchpoint, watchpoint.id);
    assert_eq!(hits[0].thread, debug_thread_id(first));
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert_eq!(inferior.threads[&second].reason, None);
    assert!(
        inferior.threads[&first].debugger_stop_pending,
        "the first thread's requested stop is still outstanding"
    );
}

#[test]
fn a_seized_thread_whose_start_precedes_its_clone_event_is_armed_before_running() {
    let mut harness = watch_harness(1);
    harness.add(0x1_2000, 8).expect("arm");
    let parent = harness.threads[0];
    harness
        .controller
        .inferior
        .as_mut()
        .expect("inferior")
        .origin = InferiorOrigin::Attached;
    harness.start_continue();
    let child = Pid::from_raw(6100);
    *harness.trace().clone.borrow_mut() = Some((child, parent));
    harness.trace().take_actions();

    // The child's first stop arrives before the debugger knows it exists.
    harness
        .controller
        .process_wait(WaitStatus::PtraceEvent(
            child,
            NixSignal::SIGTRAP,
            libc::PTRACE_EVENT_STOP,
        ))
        .expect("early child stop is retained");
    assert!(harness.trace().take_actions().is_empty());
    harness
        .controller
        .process_wait(WaitStatus::PtraceEvent(
            parent,
            NixSignal::SIGTRAP,
            libc::PTRACE_EVENT_CLONE,
        ))
        .expect("clone event");

    let actions = harness.trace().take_actions();
    let armed = actions
        .iter()
        .position(|action| action == &format!("write {child} dr7=0x90001"))
        .expect("the child was armed");
    let resumed = actions
        .iter()
        .position(|action| action == &format!("continue {child} None"))
        .expect("the child was resumed");
    assert!(armed < resumed, "{actions:?}");
    assert_eq!(harness.public_reason(), None, "no unclassifiable stop");
}
