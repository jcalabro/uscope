use crate::CodeInstanceKind;
use crate::ExceptionDisposition;
use crate::InlineFrameLookup;
use crate::LaunchOptions;
use crate::MemoryReadCompletion;
use crate::MemoryReadUnavailableReason;
use crate::Path;
use crate::PresentedFrame;
use crate::ValueExpression;
use crate::ValuePathStep;
use crate::VariableQuery;
use crate::WatchpointHit;
use crate::WatchpointSpec;
use crate::debug_info::VariableContext;
use crate::debug_info::VariableRuntime;
use crate::inspection::InspectionBudget;
use crate::protocol::{
    BreakpointHit, BreakpointSpec, ResolvedBreakpointLocation, ResumeScope, SignalPolicy,
};
use crate::unwind::FrameContext;
use crate::unwind::MemoryReader;
use crate::unwind::RegisterFile;
use std::cell::RefCell;
use std::rc::Rc;
use tokio::sync::broadcast;

use super::classify::{WatchStatus, classify_stop_evidence};
use super::frames::{default_inline_visible_count, frame_lookup_address};
use super::inspection::validate_value_expression;
use super::memory::{MemoryAccessError, read_logical_memory_with};
use super::modules::{ModuleMapping, parse_maps};
use super::native::{InspectionOps, LinuxTraceOps, queued_trap_in_status};
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
fn logical_memory_reads_keep_the_readable_bytes_of_a_partial_word() {
    // Readable memory ends `readable` bytes into the second word.
    let read = |address, size, readable| {
        read_logical_memory_with(
            VirtualAddress::new(address),
            size,
            &BTreeMap::new(),
            |current| {
                let word = u64::from_le_bytes([0, 1, 2, 3, 4, 5, 6, 7]);
                if current == 0x1008 {
                    Err(MemoryAccessError::Partial { word, readable })
                } else {
                    Ok(word)
                }
            },
        )
        .expect("partial words are typed partial results")
    };
    let incomplete = |next| MemoryReadCompletion::Incomplete {
        next_address: VirtualAddress::new(next),
        reason: MemoryReadUnavailableReason::Inaccessible,
    };

    let spanning = read(0x1006, 8, 3);
    assert_eq!(spanning.bytes, [6, 7, 0, 1, 2]);
    assert_eq!(spanning.completion, incomplete(0x100b));
    // A read that ends within the readable bytes is complete.
    let within = read(0x1009, 2, 3);
    assert_eq!(within.bytes, [1, 2]);
    assert_eq!(within.completion, MemoryReadCompletion::Complete);
    // A read that begins after them returns nothing.
    let after = read(0x100c, 2, 3);
    assert!(after.bytes.is_empty());
    assert_eq!(after.completion, incomplete(0x100c));
    // A count covering the whole word keeps its final byte, and an
    // oversized count is clamped to the word.
    for readable in [8, 9] {
        let whole = read(0x1006, 10, readable);
        assert_eq!(whole.bytes, [6, 7, 0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(whole.completion, MemoryReadCompletion::Complete);
    }
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
    fn spawn(&self, _executable: &Path, _options: LaunchOptions) -> Result<Pid> {
        self.record("spawn");
        Ok(self.pid)
    }

    fn spawn_waiter(&self, _messages: mpsc::Sender<ControllerMessage>) -> Result<Waiter> {
        self.record("spawn_waiter");
        Ok(Waiter::external())
    }

    fn process_threads(&self, _process: Pid) -> Result<Vec<Pid>> {
        Self::unexpected("process_threads")
    }

    fn seize(&self, _pid: Pid, _exit_kill: bool) -> Result<bool> {
        Self::unexpected("seize")
    }

    fn interrupt(&self, _pid: Pid) -> Result<bool> {
        Self::unexpected("interrupt")
    }

    fn detach(&self, _pid: Pid, _signal: Option<Signal>) -> Result<()> {
        Self::unexpected("detach")
    }

    fn kill(&self, pid: Pid, signal: Signal) -> Result<()> {
        assert_eq!(pid, self.pid);
        assert_eq!(signal, Signal::SIGKILL);
        self.record("kill");
        Ok(())
    }

    fn reap(&self, _pid: Pid) -> Result<()> {
        Self::unexpected("reap")
    }
    fn wait_status(&self, _pid: Pid) -> std::result::Result<WaitEvent, Errno> {
        Self::unexpected("wait_status")
    }
    fn process_start_time(&self, _process: Pid) -> Option<u64> {
        None
    }
    fn tracer_process(&self) -> i32 {
        i32::try_from(std::process::id()).expect("pid fits")
    }
    fn identify_module(&self, _mapping: &ModuleMapping) -> Option<(PathBuf, u64)> {
        None
    }
    fn load_module(&self, _path: &Path, _id: crate::ModuleImageId) -> Result<DebugInfo> {
        Self::unexpected("load_module")
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

    fn continue_execution(&self, pid: Pid, signal: Option<Signal>) -> Result<()> {
        assert_eq!(pid, self.pid);
        assert_eq!(signal, None);
        self.record("continue");
        Ok(())
    }

    fn continue_during_shutdown(&self, _pid: Pid) -> Result<()> {
        Self::unexpected("continue_during_shutdown")
    }

    fn step(&self, _pid: Pid, _signal: Option<Signal>) -> Result<()> {
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

    fn executable(&self, _pid: Pid, _address: VirtualAddress) -> Result<bool> {
        Ok(true)
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
            events: events.into(),
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
        SessionLease::detached(),
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

    controller.launch(LaunchOptions::default(), launch_reply);
    controller
        .process_wait(WaitEvent::Stopped(pid, Signal::SIGTRAP))
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
    assert!(!controller.handle_shutdown_wait(WaitEvent::Signaled(pid, Signal::SIGKILL, false,)));
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
    controller.launch(LaunchOptions::default(), launch_reply);

    assert_eq!(
        controller
            .begin_pause(process_id(pid))
            .expect("a launching inferior accepts a pause"),
        ExecutionId::new(1)
    );
    controller
        .process_wait(WaitEvent::Stopped(pid, Signal::SIGTRAP))
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
    let exception = exception_info(Signal::SIGURG);
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
            name: None,
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
            name: None,
        },
    );
    inferior.barrier = Some(StopBarrier::visible(pid, StopReason::Exception(exception)));
    let address = VirtualAddress::new(0x20);
    let breakpoint = StopReason::Breakpoint {
        address,
        hits: Arc::from([BreakpointHit {
            breakpoint: BreakpointId::new(1),
            hit_count: 1,
        }]),
    };

    controller
        .begin_visible_stop(breakpoint_thread, breakpoint.clone())
        .expect("record coincident breakpoint");

    let barrier = controller
        .inferior
        .as_ref()
        .and_then(|inferior| inferior.barrier.as_ref())
        .expect("pending thread keeps barrier active");
    assert_eq!(barrier.triggering_thread, breakpoint_thread);
    assert_eq!(barrier.reason, Some(breakpoint));
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
            selected_frames: BTreeMap::new(),
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
                    name: None,
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
fn stop_classifier_preserves_signal_and_trap_provenance() {
    let metadata = |code| {
        Ok(SignalMetadata {
            code,
            sender: (code <= 0).then_some(71),
            fault_address: None,
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
            Signal::SIGTRAP,
            metadata(libc::TRAP_TRACE),
            &ExpectedStop::UserStep {
                kind: StepKind::Instruction
            },
            None,
        ),
        ClassifiedStop::Trace { ref watch } if watch.is_empty()
    ));
    // A step across a system call completes from the call's exit path.
    assert!(matches!(
        classify(
            Signal::SIGTRAP,
            metadata(libc::TRAP_BRKPT),
            &ExpectedStop::UserStep {
                kind: StepKind::Instruction
            },
            None,
        ),
        ClassifiedStop::Trace { ref watch } if watch.is_empty()
    ));
    assert!(matches!(
        classify(
            Signal::SIGTRAP,
            metadata(libc::SI_TKILL),
            &ExpectedStop::None,
            None,
        ),
        ClassifiedStop::SignalDelivery(PendingSignal {
            signal: Signal::SIGTRAP,
            sender: Some(71),
            ..
        })
    ));
    assert!(matches!(
        classify(
            Signal::SIGSEGV,
            metadata(libc::SI_KERNEL),
            &ExpectedStop::None,
            None,
        ),
        ClassifiedStop::SignalDelivery(PendingSignal {
            signal: Signal::SIGSEGV,
            code: libc::SI_KERNEL,
            ..
        })
    ));
    assert!(matches!(
        classify(
            Signal::SIGSTOP,
            Err(Errno::EINVAL),
            &ExpectedStop::None,
            None,
        ),
        ClassifiedStop::GroupStop(Signal::SIGSTOP)
    ));
    assert!(matches!(
        classify(Signal::SIGTRAP, Err(Errno::EIO), &ExpectedStop::None, None),
        ClassifiedStop::Unclassifiable(RawStopRecord {
            siginfo: Err(Errno::EIO),
            ..
        })
    ));
    assert!(matches!(
        classify(
            Signal::SIGTRAP,
            metadata(libc::SI_KERNEL),
            &ExpectedStop::None,
            Some(VirtualAddress::new(0x1234)),
        ),
        ClassifiedStop::Breakpoint(address) if address == VirtualAddress::new(0x1234)
    ));
}

#[test]
fn stop_classifier_treats_a_thread_killed_from_its_stop_as_exiting() {
    let classify = |siginfo| {
        classify_stop_evidence(
            Signal::SIGTRAP,
            "raw-status".to_owned(),
            siginfo,
            &ExpectedStop::BreakpointRepair {
                address: VirtualAddress::new(0x1234),
            },
            false,
            false,
            None,
            WatchStatus::Absent,
        )
    };
    // SIGKILL wakes a thread from a reported stop: its siginfo vanishes and
    // then describes its exit event.
    for siginfo in [
        Err(Errno::ESRCH),
        Ok(SignalMetadata {
            code: libc::SIGTRAP | (libc::PTRACE_EVENT_EXIT << 8),
            sender: None,
            fault_address: None,
        }),
    ] {
        assert!(matches!(classify(siginfo), ClassifiedStop::Superseded));
    }
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
    /// A thread SIGKILL takes out of its stop, leaving it at its exit event,
    /// when the controller makes the named request of it for the given
    /// time, counting from zero.
    kill_point: RefCell<Option<(Pid, &'static str, u32)>>,
    /// A thread SIGKILL takes out of its stop as the controller reads the
    /// given address of it, such as a stack slot unwinding needs. That read
    /// fails while the thread runs to its exit event; once there, it answers
    /// requests again, as Linux's threads do at that stop.
    kill_at_read: RefCell<Option<(Pid, u64)>>,
    /// Threads SIGKILL took out of their stop, which refuse every request.
    killed: RefCell<BTreeSet<Pid>>,
    /// The child reported by the next clone event, in the process `tgid`.
    clone: RefCell<Option<(Pid, Pid)>>,
    /// The process's thread list.
    listed_threads: RefCell<Vec<Pid>>,
    /// Memory words by address; others read as zero.
    memory: RefCell<BTreeMap<u64, u64>>,
    /// Words whose reads fail, as unmapped memory's do.
    unreadable: RefCell<BTreeSet<u64>>,
    /// Fails every memory read operationally, unlike unmapped memory.
    read_failure: RefCell<Option<Errno>>,
}

impl DebugRegisterTrace {
    fn record(&self, action: String) {
        self.actions.borrow_mut().push(action);
    }

    /// Takes a thread out of its stop as SIGKILL does, to its exit event.
    fn sigkill(&self, pid: Pid) {
        self.killed.borrow_mut().insert(pid);
        self.vanished.borrow_mut().insert(pid);
        self.siginfo.borrow_mut().insert(
            pid,
            SignalMetadata {
                code: libc::SIGTRAP | (libc::PTRACE_EVENT_EXIT << 8),
                sender: None,
                fault_address: None,
            },
        );
    }

    /// Fails a request of a thread SIGKILL took out of its stop, which this
    /// request may be the one to do.
    fn reach(&self, pid: Pid, request: &'static str) -> Result<()> {
        let mut point = self.kill_point.borrow_mut();
        if let Some((target, name, remaining)) = point.as_mut()
            && *target == pid
            && *name == request
        {
            if *remaining == 0 {
                *point = None;
                self.sigkill(pid);
            } else {
                *remaining -= 1;
            }
        }
        if self.killed.borrow().contains(&pid) {
            return Err(backend_error(LinuxError::System(Errno::ESRCH)));
        }
        Ok(())
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
    fn read_word(&self, pid: Pid, address: u64) -> Result<u64> {
        if *self.kill_at_read.borrow() == Some((pid, address)) {
            self.kill_at_read.borrow_mut().take();
            self.siginfo.borrow_mut().insert(
                pid,
                SignalMetadata {
                    code: libc::SIGTRAP | (libc::PTRACE_EVENT_EXIT << 8),
                    sender: None,
                    fault_address: None,
                },
            );
            return Err(backend_error(LinuxError::System(Errno::ESRCH)));
        }
        self.reach(pid, "read_word")?;
        if self.unreadable.borrow().contains(&address) {
            return Err(backend_error(LinuxError::System(Errno::EIO)));
        }
        Ok(self.memory.borrow().get(&address).copied().unwrap_or(0))
    }

    fn read_memory_word(
        &self,
        pid: Pid,
        address: u64,
    ) -> std::result::Result<u64, MemoryAccessError> {
        let failure = if self.reach(pid, "read_memory_word").is_err()
            || self.vanished.borrow().contains(&pid)
        {
            Some(Errno::ESRCH)
        } else {
            *self.read_failure.borrow()
        };
        if let Some(errno) = failure {
            return Err(MemoryAccessError::Fatal(backend_error(LinuxError::System(
                errno,
            ))));
        }
        self.read_word(pid, address)
            .map_err(|_| MemoryAccessError::Inaccessible)
    }

    fn registers(&self, pid: Pid) -> Result<libc::user_regs_struct> {
        self.reach(pid, "registers")?;
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
    fn spawn(&self, _executable: &Path, _options: LaunchOptions) -> Result<Pid> {
        RecordingTrace::unexpected("spawn")
    }
    fn spawn_waiter(&self, _messages: mpsc::Sender<ControllerMessage>) -> Result<Waiter> {
        Ok(Waiter::external())
    }
    fn process_threads(&self, _process: Pid) -> Result<Vec<Pid>> {
        Ok(self.listed_threads.borrow().clone())
    }
    fn seize(&self, pid: Pid, _exit_kill: bool) -> Result<bool> {
        self.record(format!("seize {pid}"));
        Ok(true)
    }
    fn interrupt(&self, pid: Pid) -> Result<bool> {
        self.record(format!("interrupt {pid}"));
        Ok(true)
    }
    fn detach(&self, pid: Pid, signal: Option<Signal>) -> Result<()> {
        self.record(format!("detach {pid} {signal:?}"));
        Ok(())
    }
    fn kill(&self, pid: Pid, signal: Signal) -> Result<()> {
        self.record(format!("kill {pid} {signal}"));
        Ok(())
    }
    fn reap(&self, _pid: Pid) -> Result<()> {
        RecordingTrace::unexpected("reap")
    }
    fn wait_status(&self, _pid: Pid) -> std::result::Result<WaitEvent, Errno> {
        RecordingTrace::unexpected("wait_status")
    }
    fn process_start_time(&self, _process: Pid) -> Option<u64> {
        None
    }
    fn tracer_process(&self) -> i32 {
        i32::try_from(std::process::id()).expect("pid fits")
    }
    fn identify_module(&self, _mapping: &ModuleMapping) -> Option<(PathBuf, u64)> {
        None
    }
    fn load_module(&self, _path: &Path, _id: crate::ModuleImageId) -> Result<DebugInfo> {
        RecordingTrace::unexpected("load_module")
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
        Ok(0)
    }
    fn write_word(&self, pid: Pid, address: u64, value: u64) -> Result<()> {
        self.record(format!("write_word {pid} {address:#x} {value:#x}"));
        Ok(())
    }
    fn continue_execution(&self, pid: Pid, signal: Option<Signal>) -> Result<()> {
        self.record(format!("continue {pid} {signal:?}"));
        if self.vanished.borrow().contains(&pid) {
            return Err(backend_error(LinuxError::System(Errno::ESRCH)));
        }
        Ok(())
    }
    fn continue_during_shutdown(&self, _pid: Pid) -> Result<()> {
        RecordingTrace::unexpected("continue_during_shutdown")
    }
    fn step(&self, pid: Pid, _signal: Option<Signal>) -> Result<()> {
        self.record(format!("step {pid}"));
        Ok(())
    }
    fn set_registers(&self, pid: Pid, registers: libc::user_regs_struct) -> Result<()> {
        self.record(format!("set_registers {pid} rip={:#x}", registers.rip));
        Ok(())
    }
    fn set_options(&self, pid: Pid, _exit_kill: bool) -> Result<()> {
        self.record(format!("set_options {pid}"));
        self.reach(pid, "set_options")?;
        if self.vanished.borrow().contains(&pid) {
            return Err(backend_error(LinuxError::System(Errno::ESRCH)));
        }
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
        if self.vanished.borrow().contains(&thread) {
            return Err(backend_error(LinuxError::System(Errno::ESRCH)));
        }
        Ok(())
    }
    fn queued_trap(&self, pid: Pid) -> Result<bool> {
        Ok(self.queued_traps.borrow().contains(&pid))
    }
    fn executable(&self, _pid: Pid, _address: VirtualAddress) -> Result<bool> {
        Ok(true)
    }
    fn read_debug_register(&self, pid: Pid, index: usize) -> std::result::Result<u64, Errno> {
        self.record(format!("read {pid} dr{index}"));
        if self.reach(pid, "read_debug_register").is_err() {
            return Err(Errno::ESRCH);
        }
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
        pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
        owner: BreakpointOwner,
    ) -> Result<()> {
        self.reach(pid, "install_breakpoint")?;
        if let Some(site) = sites.get_mut(&address) {
            site.owners.insert(owner);
            return Ok(());
        }
        self.record(format!("install_site {address}"));
        sites.insert(
            address,
            BreakpointSite {
                original_byte: 0x90,
                installed: true,
                owners: BTreeSet::from([owner]),
            },
        );
        Ok(())
    }
    fn remove_breakpoint(
        &self,
        pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
    ) -> Result<()> {
        self.reach(pid, "remove_breakpoint")?;
        self.record(format!("remove_site {address}"));
        sites.get_mut(&address).expect("known site").installed = false;
        Ok(())
    }
    fn reinstall_breakpoint(
        &self,
        pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
    ) -> Result<()> {
        self.reach(pid, "reinstall_breakpoint")?;
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
        self.add_watching(address, byte_size, WatchAccess::Write)
    }

    fn add_watching(
        &mut self,
        address: u64,
        byte_size: u64,
        access: WatchAccess,
    ) -> Result<Watchpoint> {
        self.controller.add_watchpoint(
            WatchpointSpec::Location {
                address: VirtualAddress::new(address),
                byte_size,
            },
            access,
        )
    }

    /// Stores `value` in the word at `address`, as the inferior would.
    fn store(&self, address: u64, value: u64) {
        self.trace().memory.borrow_mut().insert(address, value);
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
                    fault_address: None,
                },
            );
            self.controller
                .process_wait(WaitEvent::Stopped(pid, Signal::SIGSTOP))
                .expect("debugger stop");
        }
    }

    /// Reports a SIGTRAP with `code` while DR6 holds `status`.
    fn trap(&mut self, pid: Pid, code: i32, status: u64) -> Result<()> {
        let mut registers = self.trace().registers_of(pid);
        registers[debug_registers::STATUS_REGISTER] = status;
        self.trace().registers.borrow_mut().insert(pid, registers);
        self.trace().siginfo.borrow_mut().insert(
            pid,
            SignalMetadata {
                code,
                sender: None,
                fault_address: None,
            },
        );
        self.controller
            .process_wait(WaitEvent::Stopped(pid, Signal::SIGTRAP))
    }

    /// Reports an int3 at `address`, leaving the PC after it.
    fn hit_at(&mut self, pid: Pid, address: u64) -> Result<()> {
        self.trace()
            .program_counters
            .borrow_mut()
            .insert(pid, address + 1);
        self.trap(pid, libc::SI_KERNEL, debug_registers::STATUS_IDLE)
    }

    /// Requests an edit and returns the receiver of its reply.
    fn edit<T>(
        &mut self,
        make: impl FnOnce(Reply<T>) -> Edit,
    ) -> tokio::sync::oneshot::Receiver<Result<T>> {
        let (reply, result) = tokio::sync::oneshot::channel();
        self.controller.edit(make(reply));
        result
    }

    /// Takes the published events other than revision changes.
    fn published(&mut self) -> Vec<DebuggerEvent> {
        std::iter::from_fn(|| self.events.try_recv().ok())
            .filter(|event| !matches!(event, DebuggerEvent::StateChanged { .. }))
            .collect()
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
    assert_eq!(
        harness.trace().take_actions(),
        [
            format!("write {exiting} dr7=0x0"),
            format!("write {exiting} dr0=0xb000"),
            format!("write {exiting} dr7=0x90001"),
            format!("read {exiting} dr7"),
            format!("read {exiting} dr0"),
        ]
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
        .process_wait(WaitEvent::Stopped(child, Signal::SIGSTOP))
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
        .process_wait(WaitEvent::Stopped(child, Signal::SIGSTOP))
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
    harness.controller.breakpoints.push(Breakpoint {
        id: BreakpointId::new(1),
        spec: BreakpointSpec::Address(VirtualAddress::new(0x40)),
        locations: Arc::from([]),
        hit_condition: None,
        condition: None,
        log_message: None,
        hit_count: 0,
    });
    harness.start_continue();
    harness.trace().take_actions();
    // An int3 stop while DR6 still records an earlier hit.
    harness
        .trap(pid, libc::SI_KERNEL, debug_registers::STATUS_IDLE | 0b1)
        .expect("int3");
    assert_eq!(
        harness.public_reason(),
        Some(StopReason::Breakpoint {
            address: VirtualAddress::new(0x40),
            hits: Arc::from([BreakpointHit {
                breakpoint: BreakpointId::new(1),
                hit_count: 1,
            }]),
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
            fault_address: None,
        },
    );
    assert!(
        harness
            .controller
            .handle_detach_wait(WaitEvent::Stopped(first, Signal::SIGTRAP))
    );
    harness.trace().take_actions();
    assert!(
        !harness
            .controller
            .handle_detach_wait(WaitEvent::PtraceEvent(
                second,
                Signal::SIGTRAP,
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
    assert!(
        matches!(
            &result,
            Err(Error::Backend(error)) if matches!(
                error.downcast_ref::<LinuxError>(),
                Some(LinuxError::System(Errno::EPERM))
            )
        ),
        "the disarm failure is reported: {result:?}"
    );

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
        .process_wait(WaitEvent::PtraceEvent(
            second,
            Signal::SIGTRAP,
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
            fault_address: None,
        },
    );
    assert!(
        !harness
            .controller
            .handle_detach_wait(WaitEvent::Stopped(pid, Signal::SIGTRAP))
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

impl WatchHarness {
    /// Makes the harness's threads those of an attach that has seized and
    /// interrupted them, and returns the attach's reply.
    fn begin_attach(&mut self) -> tokio::sync::oneshot::Receiver<Result<StopId>> {
        let (reply, attached) = tokio::sync::oneshot::channel();
        self.controller.attach_reply = Some(reply);
        let inferior = self.controller.inferior.as_mut().expect("inferior");
        inferior.origin = InferiorOrigin::Attached;
        inferior.public_stop = None;
        inferior.barrier = Some(StopBarrier::visible(inferior.tgid, StopReason::Attach));
        for thread in inferior.threads.values_mut() {
            *thread = TraceThread::starting(ExpectedStop::InitialAttach);
        }
        self.trace().listed_threads.replace(self.threads.clone());
        self.trace().take_actions();
        attached
    }

    /// Reports the `PTRACE_EVENT_STOP` an interrupt causes.
    fn interrupted(&mut self, pid: Pid) -> Result<()> {
        self.controller.process_wait(WaitEvent::PtraceEvent(
            pid,
            Signal::SIGTRAP,
            libc::PTRACE_EVENT_STOP,
        ))
    }

    /// Reports `parent` stopped at its clone event for `child`.
    fn cloned(&mut self, parent: Pid, child: Pid) -> Result<()> {
        let tgid = self.controller.inferior.as_ref().expect("inferior").tgid;
        self.trace().clone.replace(Some((child, tgid)));
        self.controller.process_wait(WaitEvent::PtraceEvent(
            parent,
            Signal::SIGTRAP,
            libc::PTRACE_EVENT_CLONE,
        ))
    }
}

#[test]
fn an_attach_traces_threads_created_while_their_creators_were_seized() {
    let mut harness = watch_harness(1);
    let leader = harness.threads[0];
    let mut attached = harness.begin_attach();
    // The leader was seized inside clone, after the kernel had decided not
    // to trace the thread it was creating, which the thread list now shows.
    let untraced = Pid::from_raw(5100);
    harness.trace().listed_threads.borrow_mut().push(untraced);

    harness.interrupted(leader).expect("leader stops");
    assert_eq!(
        harness.trace().take_actions(),
        [format!("seize {untraced}"), format!("interrupt {untraced}")]
    );
    assert!(
        attached.try_recv().is_err(),
        "{:?}",
        harness.public_reason()
    );

    harness.interrupted(untraced).expect("new thread stops");
    attached
        .try_recv()
        .expect("attach replied")
        .expect("attached");
    assert_eq!(harness.public_reason(), Some(StopReason::Attach));
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert_eq!(
        inferior.threads.keys().copied().collect::<Vec<_>>(),
        [leader, untraced]
    );
}

#[test]
fn an_interrupt_kept_past_a_clone_event_resumes_the_thread_unseen() {
    let mut harness = watch_harness(1);
    let leader = harness.threads[0];
    let child = Pid::from_raw(5001);
    let mut attached = harness.begin_attach();
    // The leader was already at its clone event when the attach interrupted
    // it. The kernel keeps such an interrupt until the thread resumes.
    harness.trace().listed_threads.borrow_mut().push(child);
    harness.cloned(leader, child).expect("clone event");
    harness.interrupted(child).expect("new thread starts");
    attached
        .try_recv()
        .expect("attach replied")
        .expect("attached");
    harness.resume().expect("continue");
    harness.trace().take_actions();
    while harness.events.try_recv().is_ok() {}

    harness.interrupted(leader).expect("kept interrupt");
    assert_eq!(
        harness.trace().take_actions(),
        [format!("continue {leader} None")]
    );
    assert_eq!(harness.public_reason(), None);
    assert_eq!(harness.published_stops(), 0);
}

#[test]
fn a_seized_thread_whose_interrupt_a_clone_event_took_is_interrupted_again() {
    let mut harness = watch_harness(2);
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };
    let child = Pid::from_raw(5100);
    harness
        .controller
        .inferior
        .as_mut()
        .expect("inferior")
        .origin = InferiorOrigin::Attached;
    harness.resume().expect("continue");
    let process = process_id(harness.controller.inferior.as_ref().expect("inferior").tgid);

    // The first thread reaches its clone event after the pause interrupts
    // it. That stop satisfies the interrupt, so no other stop follows.
    harness.controller.begin_pause(process).expect("pause");
    harness.cloned(first, child).expect("clone event");
    harness.interrupted(child).expect("new thread starts");
    harness.interrupted(second).expect("second stops");
    assert_eq!(harness.public_reason(), Some(StopReason::Pause));

    harness.resume().expect("continue");
    harness.trace().take_actions();
    harness.controller.begin_pause(process).expect("pause");
    let actions = harness.trace().take_actions();
    for pid in [first, second, child] {
        assert!(actions.contains(&format!("interrupt {pid}")), "{actions:?}");
    }
}

#[test]
fn a_fork_child_loses_each_trap_it_inherited_even_one_lifted_since() {
    let mut harness = hit_harness(1, ">=1");
    let parent = harness.threads[0];
    let child = Pid::from_raw(6000);
    harness.trace().clone.replace(Some((child, child)));
    harness
        .controller
        .process_wait(WaitEvent::PtraceEvent(
            parent,
            Signal::SIGTRAP,
            libc::PTRACE_EVENT_FORK,
        ))
        .expect("fork event");
    harness.hit(parent).expect("breakpoint stop");
    assert_eq!(harness.public_reason(), Some(site_hit(1)));
    // The parent steps over the trap it stopped at, lifted from its own
    // memory but not from the child's, when the child first stops.
    harness.resume().expect("continue");
    assert!(harness.trace().take_actions().ends_with(&[
        format!("remove_site {HIT_SITE:#x}"),
        format!("step {parent}")
    ]));

    harness
        .controller
        .process_wait(WaitEvent::Stopped(child, Signal::SIGSTOP))
        .expect("child's first stop");
    assert_eq!(
        harness.trace().take_actions(),
        [
            format!("write_word {child} {HIT_SITE:#x} 0x90"),
            format!("detach {child} None"),
        ]
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
            .process_wait(WaitEvent::Exited(pid, 0))
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
        .process_wait(WaitEvent::PtraceEvent(
            child,
            Signal::SIGTRAP,
            libc::PTRACE_EVENT_STOP,
        ))
        .expect("early child stop is retained");
    assert!(harness.trace().take_actions().is_empty());
    harness
        .controller
        .process_wait(WaitEvent::PtraceEvent(
            parent,
            Signal::SIGTRAP,
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

#[test]
fn a_new_thread_killed_out_of_its_first_stop_is_retired_by_its_exit() {
    let mut harness = watch_harness(1);
    let parent = harness.threads[0];
    harness.start_continue();
    let child = Pid::from_raw(6100);
    *harness.trace().clone.borrow_mut() = Some((child, parent));
    harness
        .controller
        .process_wait(WaitEvent::PtraceEvent(
            parent,
            Signal::SIGTRAP,
            libc::PTRACE_EVENT_CLONE,
        ))
        .expect("clone event");

    // The parent resumed and called exit_group before the child's first
    // stop was handled, killing the child out of that stop.
    harness.trace().vanished.borrow_mut().insert(child);
    harness
        .controller
        .process_wait(WaitEvent::PtraceEvent(
            child,
            Signal::SIGTRAP,
            libc::PTRACE_EVENT_STOP,
        ))
        .expect("the killed child's start is no failure");
    assert_eq!(harness.thread(child).state, NativeThreadState::Exiting);

    for pid in [child, parent] {
        harness
            .controller
            .process_wait(WaitEvent::Exited(pid, 0))
            .expect("exit");
    }
    assert!(harness.controller.inferior.is_none());
}

#[test]
fn a_requested_stop_that_sigkill_ended_since_is_superseded() {
    let mut harness = watch_harness(2);
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
    harness
        .controller
        .begin_pause(process_id(first))
        .expect("pause");

    // A sibling's exit_group killed the second thread out of the stop it
    // reported, and it now waits at its exit event.
    harness.trace().siginfo.borrow_mut().insert(
        second,
        SignalMetadata {
            code: libc::SIGTRAP | (libc::PTRACE_EVENT_EXIT << 8),
            sender: None,
            fault_address: None,
        },
    );
    harness
        .controller
        .process_wait(WaitEvent::PtraceEvent(
            second,
            Signal::SIGTRAP,
            libc::PTRACE_EVENT_STOP,
        ))
        .expect("a superseded stop is no failure");
    assert_eq!(harness.thread(second).state, NativeThreadState::Running);

    harness
        .controller
        .process_wait(WaitEvent::PtraceEvent(
            first,
            Signal::SIGTRAP,
            libc::PTRACE_EVENT_STOP,
        ))
        .expect("first stop");
    assert_eq!(harness.public_reason(), None, "the second thread runs");
    harness
        .controller
        .process_wait(WaitEvent::PtraceEvent(
            second,
            Signal::SIGTRAP,
            libc::PTRACE_EVENT_EXIT,
        ))
        .expect("exit event");
    harness
        .controller
        .process_wait(WaitEvent::Exited(second, 0))
        .expect("exit");
    assert_eq!(harness.public_reason(), Some(StopReason::Pause));
}

#[test]
fn a_pause_ends_once_a_thread_reaped_before_its_request_is_retired() {
    let mut harness = watch_harness(2);
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };
    harness.start_continue();
    // The waiter reaped the second thread, whose exit status is still on
    // its way to the controller.
    harness.trace().vanished.borrow_mut().insert(second);
    harness
        .controller
        .begin_pause(process_id(first))
        .expect("a reaped thread does not fail the pause");
    harness.trace().vanished.borrow_mut().clear();
    harness
        .controller
        .process_wait(WaitEvent::Exited(second, 0))
        .expect("exit");
    harness.settle_requested_stops();
    assert_eq!(harness.public_reason(), Some(StopReason::Pause));
}

#[test]
fn a_stop_whose_thread_sigkill_ends_while_it_is_handled_waits_for_the_exit() {
    let mut harness = watch_harness(2);
    let [stepping, sibling] = harness.threads[..] else {
        unreachable!("two threads");
    };
    harness.start_continue();
    let inferior = harness.controller.inferior.as_mut().expect("inferior");
    inferior.active.as_mut().expect("execution").kind = ActiveKind::Step {
        thread: stepping,
        kind: StepKind::OverInstruction,
        start: Box::new(StepStart {
            source: None,
            code_instance: None,
            physical_instance: None,
            activation: None,
            plan_addresses: BTreeSet::new(),
            epilogue_traversal: None,
            return_traversal: None,
            signal_guard: None,
            call_return: None,
        }),
        progress_owed: false,
    };
    inferior.thread_mut(stepping).expect("thread").expected = ExpectedStop::UserStep {
        kind: StepKind::OverInstruction,
    };

    // The sibling calls exit_group after the step's trap was classified.
    *harness.trace().kill_point.borrow_mut() = Some((stepping, "registers", 0));
    harness.trace().siginfo.borrow_mut().insert(
        stepping,
        SignalMetadata {
            code: libc::TRAP_TRACE,
            sender: None,
            fault_address: None,
        },
    );
    assert!(
        harness
            .controller
            .handle_wait(WaitEvent::Stopped(stepping, Signal::SIGTRAP))
    );
    assert!(
        harness.trace().kill_point.borrow().is_none(),
        "the handler read the killed thread's registers"
    );
    assert_eq!(harness.thread(stepping).state, NativeThreadState::Running);

    // Both threads exit, and the session ends with the process.
    for status in [
        WaitEvent::PtraceEvent(stepping, Signal::SIGTRAP, libc::PTRACE_EVENT_EXIT),
        WaitEvent::PtraceEvent(sibling, Signal::SIGTRAP, libc::PTRACE_EVENT_EXIT),
        WaitEvent::Exited(stepping, 0),
        WaitEvent::Exited(sibling, 0),
    ] {
        assert!(harness.controller.handle_wait(status));
    }
    assert!(harness.controller.inferior.is_none());
}

#[test]
fn a_step_whose_threads_sigkill_ends_as_it_begins_runs_into_the_exit() {
    let mut harness = watch_harness(2);
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };
    let stop = harness
        .controller
        .inferior
        .as_ref()
        .and_then(|inferior| inferior.public_stop.as_ref())
        .expect("stop")
        .id;
    // Something outside the debugger kills the program as the step starts.
    *harness.trace().kill_point.borrow_mut() = Some((first, "registers", 0));
    harness.trace().sigkill(second);
    let execution = harness.controller.begin_execution(
        process_id(first),
        stop,
        ResumeScope::Process(process_id(first)),
        ActiveKind::Step {
            thread: first,
            kind: StepKind::IntoSource,
            start: Box::new(StepStart {
                source: None,
                code_instance: None,
                physical_instance: None,
                activation: None,
                plan_addresses: BTreeSet::new(),
                epilogue_traversal: None,
                return_traversal: None,
                signal_guard: None,
                call_return: None,
            }),
            progress_owed: false,
        },
        ExceptionDisposition::Pass,
    );
    assert!(
        harness.trace().kill_point.borrow().is_none(),
        "the step read its thread's registers"
    );
    assert!(execution.is_ok(), "{execution:?}");
    assert!(
        !harness
            .trace()
            .take_actions()
            .iter()
            .any(|action| action.starts_with("kill")),
        "nothing kills the program"
    );
}

#[test]
fn a_trap_whose_thread_sigkill_ends_while_it_is_classified_is_superseded() {
    let mut harness = watch_harness(1);
    harness.add(0x1_1000, 8).expect("arm");
    let pid = harness.threads[0];
    harness.start_continue();
    *harness.trace().kill_point.borrow_mut() = Some((pid, "read_debug_register", 0));
    harness
        .trap(
            pid,
            TRAP_HARDWARE_BREAKPOINT,
            debug_registers::STATUS_IDLE | 0b1,
        )
        .expect("a superseded trap is no failure");
    assert!(harness.trace().kill_point.borrow().is_none());
    assert_eq!(harness.thread(pid).state, NativeThreadState::Running);
    assert_eq!(harness.public_reason(), None);
}

#[test]
fn plan_traps_a_dying_process_cannot_take_are_not_restored() {
    let plan = BTreeSet::from([VirtualAddress::new(0x50), VirtualAddress::new(0x60)]);
    // As a step begins.
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    let stop = harness
        .controller
        .inferior
        .as_ref()
        .and_then(|inferior| inferior.public_stop.as_ref())
        .expect("stop")
        .id;
    *harness.trace().kill_point.borrow_mut() = Some((pid, "install_breakpoint", 1));
    let error = harness
        .controller
        .begin_execution(
            process_id(pid),
            stop,
            ResumeScope::Process(process_id(pid)),
            ActiveKind::Step {
                thread: pid,
                kind: StepKind::OverSource,
                start: Box::new(StepStart {
                    source: None,
                    code_instance: None,
                    physical_instance: None,
                    activation: None,
                    plan_addresses: plan.clone(),
                    epilogue_traversal: None,
                    return_traversal: None,
                    signal_guard: None,
                    call_return: None,
                }),
                progress_owed: false,
            },
            ExceptionDisposition::Pass,
        )
        .expect_err("the second trap fails");
    assert!(is_vanished_tracee(&error), "{error:?}");
    assert!(
        !harness
            .trace()
            .take_actions()
            .iter()
            .any(|action| action.starts_with("kill") || action.starts_with("remove_site")),
        "nothing is restored and nothing killed"
    );

    // And as a step goes on.
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    *harness.trace().kill_point.borrow_mut() = Some((pid, "install_breakpoint", 1));
    let error = harness
        .controller
        .install_additional_plan_breakpoints(ExecutionId::new(9), &plan)
        .expect_err("the second trap fails");
    assert!(is_vanished_tracee(&error), "{error:?}");
    assert!(
        !harness
            .trace()
            .take_actions()
            .iter()
            .any(|action| action.starts_with("kill") || action.starts_with("remove_site")),
        "nothing is restored and nothing killed"
    );
}

#[test]
fn a_launch_killed_at_its_first_stop_ends_in_its_exit() {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    let inferior = harness.controller.inferior.as_mut().expect("inferior");
    inferior.public_stop = None;
    inferior.active = Some(ActiveExecution {
        id: ExecutionId::new(1),
        kind: ActiveKind::Launch,
        scope: ResumeScope::Process(process_id(pid)),
        resume_threads: BTreeSet::from([pid]),
    });
    let thread = inferior.thread_mut(pid).expect("thread");
    thread.state = NativeThreadState::Starting;
    thread.expected = ExpectedStop::InitialExec;
    harness.published();

    // Killed before its options were set.
    harness.trace().sigkill(pid);
    assert!(
        harness
            .controller
            .handle_wait(WaitEvent::Stopped(pid, Signal::SIGTRAP))
    );
    assert!(
        harness
            .controller
            .handle_wait(WaitEvent::Signaled(pid, Signal::SIGKILL, false))
    );
    assert!(harness.controller.inferior.is_none());
    assert!(
        !harness
            .trace()
            .take_actions()
            .iter()
            .any(|action| action.starts_with("kill")),
        "the debugger does not give up on the program"
    );
    assert!(harness.published().iter().any(|event| matches!(
        event,
        DebuggerEvent::InferiorExited {
            status: ExitStatus::Terminated(_),
            ..
        }
    )));
}

#[test]
fn repairs_after_the_debuggers_own_kill_are_left_undone() {
    let mut harness = watch_harness(3);
    harness
        .edit(|reply| Edit::AddBreakpoint {
            spec: address_breakpoint(0x40),
            options: Box::default(),
            reply,
        })
        .try_recv()
        .expect("reply")
        .expect("added");
    harness.start_continue();
    let [leader, repairing, running] = harness.threads[..] else {
        unreachable!("three threads");
    };
    let inferior = harness.controller.inferior.as_mut().expect("inferior");
    inferior.repairs = VecDeque::from([RepairGroup {
        address: VirtualAddress::new(0x40),
        remaining: VecDeque::from([running]),
        current: Some(repairing),
        // Lifting the trap already failed as the kill landed.
        site_removed: false,
    }]);
    let thread = inferior.thread_mut(repairing).expect("thread");
    thread.stopped_at_breakpoint = Some(VirtualAddress::new(0x40));
    thread.expected = ExpectedStop::BreakpointRepair {
        address: VirtualAddress::new(0x40),
    };
    inferior
        .thread_mut(running)
        .expect("thread")
        .stopped_at_breakpoint = Some(VirtualAddress::new(0x40));

    // The client kills the program mid-repair. When the repairing thread's
    // death arrives, the next repair has no stopped thread to lift the
    // trap through.
    let (reply, mut killed) = tokio::sync::oneshot::channel();
    harness.controller.kill(reply);
    for pid in [leader, repairing, running] {
        harness.trace().sigkill(pid);
    }
    for status in [
        WaitEvent::PtraceEvent(leader, Signal::SIGTRAP, libc::PTRACE_EVENT_EXIT),
        WaitEvent::Signaled(repairing, Signal::SIGKILL, false),
        WaitEvent::PtraceEvent(running, Signal::SIGTRAP, libc::PTRACE_EVENT_EXIT),
        WaitEvent::Signaled(running, Signal::SIGKILL, false),
        WaitEvent::Signaled(leader, Signal::SIGKILL, false),
    ] {
        assert!(harness.controller.handle_wait(status));
    }
    assert!(harness.controller.inferior.is_none());
    assert!(matches!(killed.try_recv(), Ok(Ok(()))), "the kill succeeds");
    let kills = harness
        .trace()
        .take_actions()
        .into_iter()
        .filter(|action| action.starts_with("kill"))
        .count();
    assert_eq!(kills, 1, "the debugger does not give up on its own kill");
}

#[test]
fn a_repair_whose_process_dies_leaves_its_trap_unrestored() {
    let mut harness = repairing_harness_of(1);
    let (leader, repairing) = (harness.threads[0], harness.threads[1]);
    // Something outside the debugger kills the program mid-repair. The
    // repairing thread's death leaves no thread to restore the trap through.
    harness.trace().sigkill(leader);
    harness.trace().sigkill(repairing);
    for status in [
        WaitEvent::PtraceEvent(leader, Signal::SIGTRAP, libc::PTRACE_EVENT_EXIT),
        WaitEvent::PtraceEvent(repairing, Signal::SIGTRAP, libc::PTRACE_EVENT_EXIT),
        WaitEvent::Signaled(repairing, Signal::SIGKILL, false),
        WaitEvent::Signaled(leader, Signal::SIGKILL, false),
    ] {
        assert!(harness.controller.handle_wait(status));
    }
    assert!(harness.controller.inferior.is_none());
    assert!(
        !harness
            .trace()
            .take_actions()
            .iter()
            .any(|action| action.starts_with("kill")),
        "the debugger does not give up on the program"
    );
}

/// The trap site of the hit-count harness's one user breakpoint.
const HIT_SITE: u64 = 0x40;

/// A process-wide continue whose threads can reach one user breakpoint at
/// [`HIT_SITE`] with `hit_condition`.
fn hit_harness(thread_count: i32, hit_condition: &str) -> WatchHarness {
    let mut harness = watch_harness(thread_count);
    harness.controller.breakpoints.push(Breakpoint {
        id: BreakpointId::new(1),
        spec: BreakpointSpec::Address(VirtualAddress::new(HIT_SITE)),
        locations: Arc::from([ResolvedBreakpointLocation {
            location: crate::BreakpointLocation::Virtual(VirtualAddress::new(HIT_SITE)),
            code_instances: Arc::from([]),
            library: None,
        }]),
        hit_condition: Some(hit_condition.parse().expect("test hit condition")),
        condition: None,
        log_message: None,
        hit_count: 0,
    });
    harness
        .controller
        .inferior
        .as_mut()
        .expect("inferior")
        .breakpoints
        .insert(
            VirtualAddress::new(HIT_SITE),
            BreakpointSite {
                original_byte: 0x90,
                installed: true,
                owners: BTreeSet::from([BreakpointOwner::User(BreakpointId::new(1))]),
            },
        );
    harness.start_continue();
    harness.trace().take_actions();
    harness
}

impl WatchHarness {
    /// Reports `pid` executing the trap at [`HIT_SITE`].
    fn hit(&mut self, pid: Pid) -> Result<()> {
        self.trace()
            .program_counters
            .borrow_mut()
            .insert(pid, HIT_SITE + 1);
        self.trap(pid, libc::SI_KERNEL, debug_registers::STATUS_IDLE)
    }

    /// Completes the single step a breakpoint repair began.
    fn finish_step(&mut self, pid: Pid) -> Result<()> {
        self.trap(pid, libc::TRAP_TRACE, debug_registers::STATUS_IDLE)
    }

    fn hit_count(&self) -> u64 {
        self.controller.breakpoints[0].hit_count
    }

    fn published_stops(&mut self) -> usize {
        std::iter::from_fn(|| self.events.try_recv().ok())
            .filter(|event| matches!(event, DebuggerEvent::InferiorStopped { .. }))
            .count()
    }

    fn resume(&mut self) -> Result<ExecutionId> {
        let inferior = self.controller.inferior.as_ref().expect("inferior");
        let process = process_id(inferior.tgid);
        let stop = inferior.public_stop.as_ref().expect("public stop").id;
        self.controller.begin_execution(
            process,
            stop,
            ResumeScope::Process(process),
            ActiveKind::Continue,
            ExceptionDisposition::Pass,
        )
    }
}

fn site_hit(hit_count: u64) -> StopReason {
    StopReason::Breakpoint {
        address: VirtualAddress::new(HIT_SITE),
        hits: Arc::from([BreakpointHit {
            breakpoint: BreakpointId::new(1),
            hit_count,
        }]),
    }
}

#[test]
fn declined_hits_lift_the_trap_only_once_every_sibling_stopped() {
    let mut harness = hit_harness(3, "==2");
    let [first, second, third] = harness.threads[..] else {
        panic!("three threads");
    };

    harness.hit(first).expect("declined hit");
    let actions = harness.trace().take_actions();
    assert!(
        actions.contains(&format!("request_stop {second}")),
        "{actions:?}"
    );
    assert!(
        actions.contains(&format!("request_stop {third}")),
        "{actions:?}"
    );
    assert!(
        !actions
            .iter()
            .any(|action| action.starts_with("remove_site") || action.starts_with("step")),
        "the trap was lifted while siblings ran: {actions:?}"
    );

    harness.settle_requested_stops();
    let actions = harness.trace().take_actions();
    let repair = [
        format!("remove_site {HIT_SITE:#x}"),
        format!("step {first}"),
    ];
    assert!(actions.ends_with(&repair), "{actions:?}");

    harness.finish_step(first).expect("repair step");
    let actions = harness.trace().take_actions();
    let resumed = [
        format!("reinstall_site {HIT_SITE:#x}"),
        format!("continue {first} None"),
        format!("continue {second} None"),
        format!("continue {third} None"),
    ];
    assert!(actions.ends_with(&resumed), "{actions:?}");
    assert_eq!(harness.published_stops(), 0);
    assert_eq!(harness.public_reason(), None);
    assert_eq!(harness.hit_count(), 1);

    // The next arrival meets the condition and stops every thread.
    harness.hit(second).expect("stopping hit");
    harness.settle_requested_stops();
    assert_eq!(harness.public_reason(), Some(site_hit(2)));
    assert_eq!(harness.published_stops(), 1);
}

#[test]
fn a_visible_stop_during_an_internal_stop_wins_and_the_declined_hit_counts_once() {
    let mut harness = hit_harness(2, "==5");
    let [first, second] = harness.threads[..] else {
        panic!("two threads");
    };
    harness.hit(first).expect("declined hit");

    // The sibling reports a signal before the stop it was asked for.
    harness.trace().siginfo.borrow_mut().insert(
        second,
        SignalMetadata {
            code: 0,
            sender: Some(1),
            fault_address: None,
        },
    );
    harness
        .controller
        .process_wait(WaitEvent::Stopped(second, Signal::SIGUSR1))
        .expect("signal stop");
    assert!(matches!(
        harness.public_reason(),
        Some(StopReason::Exception(exception)) if exception.code == Signal::SIGUSR1.code()
    ));
    let declined = harness.thread(first);
    assert_eq!(declined.reason, None);
    assert_eq!(
        declined.stopped_at_breakpoint,
        Some(VirtualAddress::new(HIT_SITE))
    );
    harness.trace().take_actions();

    // Resuming steps the declined thread over the site before anything runs.
    harness.resume().expect("resume");
    let actions = harness.trace().take_actions();
    assert!(
        actions.ends_with(&[
            format!("remove_site {HIT_SITE:#x}"),
            format!("step {first}")
        ]),
        "{actions:?}"
    );
    harness.finish_step(first).expect("repair step");
    let actions = harness.trace().take_actions();
    assert!(
        actions.ends_with(&[
            format!("reinstall_site {HIT_SITE:#x}"),
            format!("continue {first} None"),
            format!("continue {second} Some(SIGUSR1)"),
        ]),
        "{actions:?}"
    );
    assert_eq!(harness.hit_count(), 1);
}

#[test]
fn a_pause_requested_during_an_internal_stop_is_published() {
    let mut harness = hit_harness(2, "==5");
    let first = harness.threads[0];
    harness.hit(first).expect("declined hit");
    let process = process_id(harness.controller.inferior.as_ref().expect("inferior").tgid);
    harness.controller.begin_pause(process).expect("pause");
    harness.settle_requested_stops();

    assert_eq!(harness.public_reason(), Some(StopReason::Pause));
    assert!(
        !harness
            .trace()
            .take_actions()
            .iter()
            .any(|action| action.starts_with("remove_site")),
        "a pause must not repair or resume anything"
    );
    assert_eq!(
        harness.thread(first).stopped_at_breakpoint,
        Some(VirtualAddress::new(HIT_SITE))
    );
}

#[test]
fn a_co_hit_meeting_its_condition_turns_an_internal_stop_visible() {
    let mut harness = hit_harness(2, "==2");
    let [first, second] = harness.threads[..] else {
        panic!("two threads");
    };
    harness.hit(first).expect("declined hit");
    // The sibling trapped at the same site before its requested stop.
    harness.hit(second).expect("stopping co-hit");

    assert_eq!(harness.public_reason(), Some(site_hit(2)));
    let stop = harness
        .controller
        .inferior
        .as_ref()
        .and_then(|inferior| inferior.public_stop.as_ref())
        .expect("published stop");
    assert_eq!(stop.triggering_thread, second);
    assert_eq!(harness.thread(first).reason, None);
    assert_eq!(harness.thread(second).reason, Some(site_hit(2)));
}

#[test]
fn a_declined_thread_exiting_during_an_internal_stop_publishes_nothing() {
    let mut harness = hit_harness(3, "==5");
    let [first, second, third] = harness.threads[..] else {
        panic!("three threads");
    };
    // The kernel reports the leader's exit only after every other thread's,
    // so a sibling exits here.
    harness.hit(second).expect("declined hit");
    harness
        .controller
        .process_wait(WaitEvent::Exited(second, 0))
        .expect("thread exit");
    harness.trace().take_actions();
    harness.settle_requested_stops();

    let actions = harness.trace().take_actions();
    assert!(
        actions.ends_with(&[
            format!("continue {first} None"),
            format!("continue {third} None"),
        ]),
        "{actions:?}"
    );
    assert!(!actions.iter().any(|action| action.starts_with("step")));
    assert_eq!(harness.published_stops(), 0);
}

#[test]
fn removing_a_breakpoint_releases_threads_waiting_to_step_over_it() {
    let mut harness = hit_harness(2, "==5");
    let [first, second] = harness.threads[..] else {
        panic!("two threads");
    };
    harness.hit(first).expect("declined hit");
    let process = process_id(harness.controller.inferior.as_ref().expect("inferior").tgid);
    harness.controller.begin_pause(process).expect("pause");
    harness.settle_requested_stops();
    harness
        .controller
        .remove_breakpoint(BreakpointId::new(1))
        .expect("remove");
    assert_eq!(harness.thread(first).stopped_at_breakpoint, None);
    harness.trace().take_actions();

    harness.resume().expect("resume");
    assert_eq!(
        harness.trace().take_actions(),
        [
            format!("continue {first} None"),
            format!("continue {second} None")
        ]
    );
}

#[test]
fn amending_a_hit_condition_while_running_applies_to_the_next_hit() {
    let mut harness = hit_harness(1, "==5");
    let pid = harness.threads[0];
    harness.hit(pid).expect("declined hit");
    harness.finish_step(pid).expect("repair step");
    assert_eq!(harness.public_reason(), None);

    let amended = harness
        .controller
        .set_breakpoint_hit_condition(BreakpointId::new(1), Some(">=2".parse().expect("valid")))
        .expect("amend while running");
    assert_eq!(amended.hit_count, 1);
    harness.hit(pid).expect("stopping hit");
    assert_eq!(harness.public_reason(), Some(site_hit(2)));
}

#[test]
fn a_trap_reexecuted_after_a_signal_interrupted_its_repair_is_not_a_new_hit() {
    let mut harness = hit_harness(1, "==2");
    let pid = harness.threads[0];
    harness.hit(pid).expect("declined hit");
    // A signal arrives instead of the repair step's trace trap, before the
    // original instruction ran.
    harness.trace().siginfo.borrow_mut().insert(
        pid,
        SignalMetadata {
            code: 0,
            sender: Some(1),
            fault_address: None,
        },
    );
    harness
        .controller
        .process_wait(WaitEvent::Stopped(pid, Signal::SIGUSR1))
        .expect("signal during repair");
    assert!(matches!(
        harness.public_reason(),
        Some(StopReason::Exception(_))
    ));
    harness.trace().take_actions();

    harness.resume().expect("resume");
    assert_eq!(
        harness.trace().take_actions(),
        [format!("continue {pid} Some(SIGUSR1)")]
    );
    // After its handler returns the thread executes the same trap again.
    harness.hit(pid).expect("re-executed trap");
    assert_eq!(harness.public_reason(), None, "the hit was counted twice");
    harness.finish_step(pid).expect("repair step");
    assert_eq!(harness.hit_count(), 1);
    assert_eq!(
        harness.trace().take_actions().last(),
        Some(&format!("continue {pid} None"))
    );
}

fn address_breakpoint(address: u64) -> BreakpointSpec {
    BreakpointSpec::Address(VirtualAddress::new(address))
}

#[test]
fn an_edit_while_running_applies_at_an_internal_stop_and_resumes_silently() {
    let mut harness = watch_harness(2);
    harness.start_continue();
    harness.published();
    let mut added = harness.edit(|reply| Edit::AddBreakpoint {
        spec: address_breakpoint(0x40),
        options: Box::default(),
        reply,
    });

    assert_eq!(
        harness.trace().take_actions(),
        ["request_stop 5000", "request_stop 5001"]
    );
    assert!(added.try_recv().is_err(), "the edit waits for every thread");
    harness.settle_requested_stops();

    let breakpoint = added.try_recv().expect("edit reply").expect("added");
    assert_eq!(breakpoint.id, BreakpointId::new(1));
    assert_eq!(
        harness.trace().take_actions(),
        [
            "install_site 0x40",
            "continue 5000 None",
            "continue 5001 None"
        ]
    );
    assert!(
        matches!(
            harness.published().as_slice(),
            [DebuggerEvent::BreakpointsChanged { .. }]
        ),
        "an internal stop publishes no stop or continue"
    );
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert!(inferior.public_stop.is_none() && inferior.barrier.is_none());
    assert_eq!(
        inferior.active.as_ref().map(|active| active.id),
        Some(ExecutionId::new(2)),
        "the execution continues under its own identity"
    );
    assert!(
        inferior
            .threads
            .values()
            .all(|thread| thread.state == NativeThreadState::Running)
    );
}

#[test]
fn a_hit_on_a_breakpoint_removed_while_running_is_dropped() {
    let mut harness = watch_harness(2);
    let breakpoint = harness
        .edit(|reply| Edit::AddBreakpoint {
            spec: address_breakpoint(0x40),
            options: Box::default(),
            reply,
        })
        .try_recv()
        .expect("reply")
        .expect("added at the stop");
    harness.start_continue();
    harness.trace().take_actions();
    harness.published();

    let mut removed = harness.edit(|reply| Edit::RemoveBreakpoint {
        id: breakpoint.id,
        reply,
    });
    // The trap was raised before the thread saw its stop request.
    harness
        .hit_at(Pid::from_raw(5000), 0x40)
        .expect("breakpoint trap");
    harness.settle_requested_stops();

    // The hit counted, but the breakpoint is gone before it stops anything.
    let removed = removed.try_recv().expect("reply").expect("removed");
    assert_eq!((removed.id, removed.hit_count), (breakpoint.id, 1));
    let actions = harness.trace().take_actions();
    assert_eq!(
        &actions[actions.len() - 3..],
        [
            "remove_site 0x40",
            "continue 5000 None",
            "continue 5001 None"
        ],
        "the thread resumes the restored instruction without a repair: {actions:?}"
    );
    assert!(
        !harness
            .published()
            .iter()
            .any(|event| matches!(event, DebuggerEvent::InferiorStopped { .. })),
        "a removed breakpoint's hit is never published"
    );
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert!(inferior.breakpoints.is_empty());
    assert!(
        inferior
            .threads
            .values()
            .all(|thread| thread.stopped_at_breakpoint.is_none() && thread.reason.is_none())
    );
}

#[test]
fn a_hit_on_a_breakpoint_that_survives_the_edit_is_published_with_its_owners() {
    let mut harness = watch_harness(2);
    let kept = harness
        .edit(|reply| Edit::AddBreakpoint {
            spec: address_breakpoint(0x40),
            options: Box::default(),
            reply,
        })
        .try_recv()
        .expect("reply")
        .expect("added");
    let other = harness
        .edit(|reply| Edit::AddBreakpoint {
            spec: address_breakpoint(0x48),
            options: Box::default(),
            reply,
        })
        .try_recv()
        .expect("reply")
        .expect("added");
    harness.start_continue();
    harness.published();

    let mut removed = harness.edit(|reply| Edit::RemoveBreakpoint {
        id: other.id,
        reply,
    });
    harness
        .hit_at(Pid::from_raw(5001), 0x40)
        .expect("breakpoint trap");
    harness.settle_requested_stops();

    removed.try_recv().expect("reply").expect("removed");
    let published = harness.published();
    let position = |predicate: fn(&DebuggerEvent) -> bool| {
        published
            .iter()
            .position(predicate)
            .unwrap_or_else(|| panic!("missing event in {published:?}"))
    };
    assert!(
        position(|event| matches!(event, DebuggerEvent::BreakpointsChanged { .. }))
            < position(|event| matches!(event, DebuggerEvent::InferiorStopped { .. })),
        "the edit applies before the stop is published"
    );
    assert!(published.iter().any(|event| matches!(
        event,
        DebuggerEvent::InferiorStopped {
            execution_id: Some(execution),
            thread_id,
            reason: StopReason::Breakpoint { address, hits },
            ..
        } if *execution == ExecutionId::new(2)
            && thread_id.get() == 5001
            && address.get() == 0x40
            && hits.iter().map(|hit| hit.breakpoint).eq([kept.id])
    )));
}

#[test]
fn a_signal_during_an_internal_stop_is_published_after_the_edit() {
    let mut harness = watch_harness(2);
    harness.start_continue();
    harness.published();
    let mut added = harness.edit(|reply| Edit::AddBreakpoint {
        spec: address_breakpoint(0x40),
        options: Box::default(),
        reply,
    });
    let pid = Pid::from_raw(5001);
    harness.trace().siginfo.borrow_mut().insert(
        pid,
        SignalMetadata {
            code: libc::SI_USER,
            sender: Some(1),
            fault_address: None,
        },
    );
    harness
        .controller
        .process_wait(WaitEvent::Stopped(pid, Signal::SIGUSR1))
        .expect("signal stop");
    harness.settle_requested_stops();

    added.try_recv().expect("reply").expect("added");
    assert!(matches!(
        harness.public_reason(),
        Some(StopReason::Exception(info)) if info.code == Signal::SIGUSR1.code()
    ));
    assert!(
        harness
            .trace()
            .take_actions()
            .contains(&"install_site 0x40".to_owned())
    );
}

#[test]
fn pausing_during_an_internal_stop_publishes_the_pause() {
    let mut harness = watch_harness(2);
    harness.start_continue();
    let process = process_id(harness.threads[0]);
    let mut added = harness.edit(|reply| Edit::AddBreakpoint {
        spec: address_breakpoint(0x40),
        options: Box::default(),
        reply,
    });
    assert_eq!(
        harness.controller.begin_pause(process).expect("pause"),
        ExecutionId::new(2)
    );
    harness.settle_requested_stops();

    added.try_recv().expect("reply").expect("added");
    assert_eq!(harness.public_reason(), Some(StopReason::Pause));
}

#[test]
fn another_thread_at_a_stepping_plans_site_is_stepped_over_while_the_others_are_stopped() {
    let mut harness = watch_harness(2);
    let (stepping, other) = (harness.threads[0], harness.threads[1]);
    harness.start_continue();
    {
        let inferior = harness.controller.inferior.as_mut().expect("inferior");
        let active = inferior.active.as_mut().expect("execution");
        active.kind = ActiveKind::Step {
            thread: stepping,
            kind: StepKind::OverSource,
            start: Box::new(StepStart {
                source: None,
                code_instance: None,
                physical_instance: None,
                activation: None,
                plan_addresses: BTreeSet::from([VirtualAddress::new(0x40)]),
                epilogue_traversal: None,
                return_traversal: None,
                signal_guard: None,
                call_return: None,
            }),
            progress_owed: false,
        };
        let execution = active.id;
        let owner = BreakpointOwner::Plan(execution);
        inferior
            .plan_sites
            .insert(execution, BTreeSet::from([VirtualAddress::new(0x40)]));
        inferior.breakpoints.insert(
            VirtualAddress::new(0x40),
            BreakpointSite {
                original_byte: 0x90,
                installed: true,
                owners: BTreeSet::from([owner]),
            },
        );
    }
    harness.published();
    harness.trace().take_actions();

    harness.hit_at(other, 0x40).expect("plan site trap");
    assert_eq!(
        harness.trace().take_actions(),
        ["set_registers 5001 rip=0x40", "request_stop 5000"],
        "the stepping thread stops before the site is lifted"
    );
    harness.settle_requested_stops();
    assert_eq!(
        harness.trace().take_actions(),
        ["remove_site 0x40", "step 5001"]
    );
    harness
        .trap(other, libc::TRAP_TRACE, debug_registers::STATUS_IDLE)
        .expect("repair step");

    assert_eq!(
        harness.trace().take_actions(),
        [
            "reinstall_site 0x40",
            "continue 5000 None",
            "continue 5001 None"
        ]
    );
    assert!(
        !harness
            .published()
            .iter()
            .any(|event| matches!(event, DebuggerEvent::InferiorStopped { .. })),
        "the stepping plan's site is never reported for another thread"
    );
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert!(inferior.barrier.is_none() && inferior.repairs.is_empty());
    assert_eq!(
        inferior.active.as_ref().map(|active| active.id),
        Some(ExecutionId::new(2))
    );
}

#[test]
fn ending_a_plan_removes_only_the_sites_it_still_owns() {
    let mut harness = watch_harness(1);
    let (ending, other) = (ExecutionId::new(7), ExecutionId::new(8));
    let user = BreakpointOwner::User(BreakpointId::new(1));
    let address = VirtualAddress::new;
    {
        let ptrace = &harness.controller.ptrace;
        let inferior = harness.controller.inferior.as_mut().expect("inferior");
        for site in [0x20, 0x30, 0x40] {
            breakpoints::install_plan_breakpoint(ptrace, inferior, address(site), ending)
                .expect("plan site");
        }
        breakpoints::install_plan_breakpoint(ptrace, inferior, address(0x50), other)
            .expect("other plan's site");
        let pid = inferior.memory_thread();
        ptrace
            .install_breakpoint(pid, &mut inferior.breakpoints, address(0x40), user)
            .expect("user site");
        // A plan may release a site before it ends, as a finished guard does.
        breakpoints::remove_breakpoint_owner_from(
            ptrace,
            inferior,
            address(0x20),
            BreakpointOwner::Plan(ending),
        )
        .expect("released");
    }
    harness.trace().take_actions();

    harness
        .controller
        .cleanup_plan_breakpoints(ending)
        .expect("cleanup");

    assert_eq!(harness.trace().take_actions(), ["remove_site 0x30"]);
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    let owners = inferior
        .breakpoints
        .iter()
        .map(|(&site, owners)| (site, owners.owners.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        owners,
        [
            (address(0x40), BTreeSet::from([user])),
            (
                address(0x50),
                BTreeSet::from([BreakpointOwner::Plan(other)])
            ),
        ]
    );
    assert_eq!(
        inferior.plan_sites.keys().copied().collect::<Vec<_>>(),
        [other]
    );
}

#[test]
fn a_hit_on_a_watchpoint_removed_while_running_is_dropped() {
    let mut harness = watch_harness(2);
    let watchpoint = harness.add(0xd000, 8).expect("armed");
    harness.start_continue();
    harness.published();

    let mut removed = harness.edit(|reply| Edit::RemoveWatchpoint {
        id: watchpoint.id,
        reply,
    });
    harness
        .trap(
            harness.threads[0],
            TRAP_HARDWARE_BREAKPOINT,
            debug_registers::STATUS_IDLE | 0b1,
        )
        .expect("watch trap");
    harness.settle_requested_stops();

    assert_eq!(
        removed.try_recv().expect("reply").expect("removed"),
        watchpoint
    );
    let published = harness.published();
    assert!(
        !published
            .iter()
            .any(|event| matches!(event, DebuggerEvent::InferiorStopped { .. })),
        "a removed watchpoint's hit is never published: {published:?}"
    );
    assert!(
        published
            .iter()
            .any(|event| matches!(event, DebuggerEvent::WatchpointsChanged { .. }))
    );
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert!(inferior.watch.watchpoints.is_empty() && inferior.barrier.is_none());
    assert!(
        inferior.threads.values().all(
            |thread| thread.state == NativeThreadState::Running && thread.watch_hits.is_empty()
        )
    );
}

/// The bytes of a watched word holding `value`.
fn word(value: u64) -> Arc<[u8]> {
    Arc::from(value.to_le_bytes())
}

/// DR6 after a single step whose instruction hit slot 0.
const STEPPED_SLOT_ZERO: u64 = debug_registers::STATUS_IDLE | 1 << 14 | 0b1;

#[test]
fn a_store_that_changes_nothing_resumes_only_its_own_thread() {
    let mut harness = watch_harness(2);
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };
    harness.store(0x1_3000, 7);
    let watchpoint = harness
        .add_watching(0x1_3000, 8, WatchAccess::Change)
        .expect("arm");
    // Its slot reports stores only, as a write watchpoint's does.
    assert_eq!(harness.trace().registers_of(first)[7], 0x90001);
    harness.start_continue();
    harness.published();
    harness.trace().take_actions();

    harness
        .trap(
            first,
            TRAP_HARDWARE_BREAKPOINT,
            debug_registers::STATUS_IDLE | 0b1,
        )
        .expect("unchanged store");
    let actions = harness.trace().take_actions();
    assert_eq!(
        actions.last(),
        Some(&format!("continue {first} None")),
        "{actions:?}"
    );
    assert!(
        !actions
            .iter()
            .any(|action| action.starts_with("request_stop")),
        "no sibling stops for a store that changed nothing: {actions:?}"
    );
    assert_eq!(harness.published(), []);
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert!(inferior.barrier.is_none());
    assert!(inferior.threads.values().all(|thread| {
        thread.state == NativeThreadState::Running && thread.watch_hits.is_empty()
    }));

    // A store of another value stops every thread and reports the change
    // from the value observed when execution resumed.
    harness.store(0x1_3000, 9);
    harness
        .trap(
            second,
            TRAP_HARDWARE_BREAKPOINT,
            debug_registers::STATUS_IDLE | 0b1,
        )
        .expect("changing store");
    harness.settle_requested_stops();
    assert_eq!(
        harness.public_reason(),
        Some(StopReason::Watchpoint {
            hits: Arc::from([WatchpointHit {
                watchpoint: watchpoint.id,
                thread: debug_thread_id(second),
                previous: Some(word(7)),
                current: Some(word(9)),
            }]),
        })
    );
}

#[test]
fn a_change_undone_before_every_thread_stopped_lets_a_stepi_finish_once() {
    let mut harness = watch_harness(2);
    let [stepping, sibling] = harness.threads[..] else {
        unreachable!("two threads");
    };
    harness.store(0x1_4000, 7);
    harness
        .add_watching(0x1_4000, 8, WatchAccess::Change)
        .expect("arm");
    harness.start_continue();
    let inferior = harness.controller.inferior.as_mut().expect("inferior");
    inferior.active.as_mut().expect("execution").kind = ActiveKind::Step {
        thread: stepping,
        kind: StepKind::Instruction,
        start: Box::new(StepStart {
            source: None,
            code_instance: None,
            physical_instance: None,
            activation: None,
            plan_addresses: BTreeSet::new(),
            epilogue_traversal: None,
            return_traversal: None,
            signal_guard: None,
            call_return: None,
        }),
        progress_owed: false,
    };
    inferior.thread_mut(stepping).expect("thread").expected = ExpectedStop::UserStep {
        kind: StepKind::Instruction,
    };
    harness.published();

    // The stepped instruction changes the value, so every thread is stopped
    // to report it, but the sibling stores the old value back first.
    harness.store(0x1_4000, 9);
    harness
        .trap(stepping, libc::TRAP_TRACE, STEPPED_SLOT_ZERO)
        .expect("stepped store");
    assert_eq!(harness.public_reason(), None);
    harness.trace().take_actions();
    harness.store(0x1_4000, 7);
    harness
        .trap(
            sibling,
            TRAP_HARDWARE_BREAKPOINT,
            debug_registers::STATUS_IDLE | 0b1,
        )
        .expect("restoring store");

    // Nothing changed once every thread stopped: the step ends after the one
    // instruction it executed, without executing another.
    assert_eq!(
        harness.public_reason(),
        Some(StopReason::Step {
            kind: StepKind::Instruction
        })
    );
    let actions = harness.trace().take_actions();
    assert!(
        !actions
            .iter()
            .any(|action| action.starts_with("step") || action.starts_with("continue")),
        "{actions:?}"
    );
    let stops = harness
        .published()
        .into_iter()
        .filter(|event| matches!(event, DebuggerEvent::InferiorStopped { .. }))
        .count();
    assert_eq!(stops, 1);
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert!(
        inferior
            .threads
            .values()
            .all(|thread| thread.watch_hits.is_empty())
    );
    assert_eq!(inferior.threads[&sibling].reason, None);
}

#[test]
fn a_pause_survives_the_change_it_coincided_with_being_undone() {
    let mut harness = watch_harness(2);
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };
    harness.store(0x1_9000, 7);
    harness
        .add_watching(0x1_9000, 8, WatchAccess::Change)
        .expect("arm");
    harness.start_continue();
    harness.store(0x1_9000, 9);
    harness
        .trap(
            first,
            TRAP_HARDWARE_BREAKPOINT,
            debug_registers::STATUS_IDLE | 0b1,
        )
        .expect("changing store");
    harness
        .controller
        .begin_pause(process_id(first))
        .expect("pause while every thread stops");
    harness.store(0x1_9000, 7);
    harness
        .trap(
            second,
            TRAP_HARDWARE_BREAKPOINT,
            debug_registers::STATUS_IDLE | 0b1,
        )
        .expect("restoring store");

    // The change is gone, but the client still asked for a stop.
    assert_eq!(harness.public_reason(), Some(StopReason::Pause));
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert!(
        inferior
            .threads
            .values()
            .all(|thread| thread.reason.is_none())
    );
}

#[test]
fn a_store_reports_only_the_watchpoints_whose_access_it_matches() {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    harness.store(0x1_5000, 7);
    let change = harness
        .add_watching(0x1_5000, 8, WatchAccess::Change)
        .expect("change");
    let write = harness
        .add_watching(0x1_5000, 8, WatchAccess::Write)
        .expect("write");
    for (stored, reported) in [(7, vec![write.id]), (8, vec![change.id, write.id])] {
        harness.start_continue();
        harness.store(0x1_5000, stored);
        // Watchpoints on the same span share its slot.
        harness
            .trap(
                pid,
                TRAP_HARDWARE_BREAKPOINT,
                debug_registers::STATUS_IDLE | 0b1,
            )
            .expect("store");
        let Some(StopReason::Watchpoint { hits }) = harness.public_reason() else {
            panic!("the write watchpoint reports every store");
        };
        assert_eq!(
            hits.iter().map(|hit| hit.watchpoint).collect::<Vec<_>>(),
            reported
        );
        assert!(hits.iter().all(|hit| hit.current == Some(word(stored))));
    }
}

#[test]
fn an_unchanged_store_while_stepping_over_a_breakpoint_finishes_the_repair_and_runs_on() {
    // Linux reports the step's own trap; a hardware trap alone must not make
    // the thread step the instruction again either.
    for (code, status) in [
        (libc::TRAP_TRACE, STEPPED_SLOT_ZERO),
        (TRAP_HARDWARE_BREAKPOINT, debug_registers::STATUS_IDLE | 0b1),
    ] {
        let mut harness = repairing_harness();
        let [repairing, waiting] = harness.threads[..] else {
            unreachable!("two threads");
        };
        harness.store(0x1_6000, 7);
        harness.thread(repairing).state = NativeThreadState::Stopped;
        harness
            .add_watching(0x1_6000, 8, WatchAccess::Change)
            .expect("arm");
        harness.thread(repairing).state = NativeThreadState::Running;
        harness.trace().take_actions();
        harness.published();

        harness.trap(repairing, code, status).expect("repair step");
        let actions = harness.trace().take_actions();
        assert!(
            actions.ends_with(&[
                "reinstall_site 0x40".to_owned(),
                format!("continue {repairing} None"),
                format!("continue {waiting} None"),
            ]),
            "{code}: {actions:?}"
        );
        assert_eq!(harness.published(), []);
        let inferior = harness.controller.inferior.as_ref().expect("inferior");
        assert!(inferior.repairs.is_empty());
        assert_eq!(inferior.threads[&repairing].stopped_at_breakpoint, None);
    }
}

#[test]
fn watched_bytes_becoming_unreadable_or_readable_are_changes() {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    harness.store(0x1_8000, 7);
    let watchpoint = harness
        .add_watching(0x1_8000, 8, WatchAccess::Change)
        .expect("arm");
    for (readable, previous, current) in [(false, Some(word(7)), None), (true, None, Some(word(7)))]
    {
        harness.start_continue();
        if readable {
            harness.trace().unreadable.borrow_mut().clear();
        } else {
            harness.trace().unreadable.borrow_mut().insert(0x1_8000);
        }
        harness
            .trap(
                pid,
                TRAP_HARDWARE_BREAKPOINT,
                debug_registers::STATUS_IDLE | 0b1,
            )
            .expect("store");
        assert_eq!(
            harness.public_reason(),
            Some(StopReason::Watchpoint {
                hits: Arc::from([WatchpointHit {
                    watchpoint: watchpoint.id,
                    thread: debug_thread_id(pid),
                    previous,
                    current,
                }]),
            }),
            "readable: {readable}"
        );
    }
}

#[test]
fn a_failed_read_of_watched_bytes_is_an_error_rather_than_a_change() {
    let mut harness = watch_harness(2);
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };
    let is_eperm = |error: &Error| {
        matches!(
            error,
            Error::Backend(error)
                if matches!(error.downcast_ref::<LinuxError>(), Some(LinuxError::System(Errno::EPERM)))
        )
    };
    harness.store(0x1_9000, 7);
    // A thread killed out of its stop is passed over for one that can read.
    harness.trace().vanished.borrow_mut().insert(first);
    let watchpoint = harness
        .add_watching(0x1_9000, 8, WatchAccess::Change)
        .expect("arm");
    harness.trace().vanished.borrow_mut().clear();
    assert_eq!(
        harness
            .controller
            .inferior
            .as_ref()
            .expect("inferior")
            .watch
            .watchpoints[&watchpoint.id]
            .observed,
        Some(word(7))
    );

    // Any other failure is never taken for unreadable bytes: not when a
    // watchpoint is armed, which then arms nothing,
    harness.trace().read_failure.replace(Some(Errno::EPERM));
    let registers = harness.trace().registers_of(first);
    let error = harness
        .add_watching(0x1_a000, 8, WatchAccess::Change)
        .expect_err("unread baseline");
    assert!(is_eperm(&error), "{error:?}");
    assert_eq!(harness.trace().registers_of(first), registers);
    // not when execution resumes,
    let error = harness.resume().expect_err("unread baseline");
    assert!(is_eperm(&error), "{error:?}");
    // and not when a store traps.
    harness.start_continue();
    harness.published();
    let error = harness
        .trap(
            second,
            TRAP_HARDWARE_BREAKPOINT,
            debug_registers::STATUS_IDLE | 0b1,
        )
        .expect_err("unread store");
    assert!(is_eperm(&error), "{error:?}");
    assert_eq!(harness.published(), []);
}

#[test]
fn an_unchanged_store_during_a_pause_gives_its_thread_no_reason() {
    let mut harness = watch_harness(2);
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };
    harness.store(0x1_7000, 7);
    harness
        .add_watching(0x1_7000, 8, WatchAccess::Change)
        .expect("arm");
    harness.start_continue();
    harness
        .controller
        .begin_pause(process_id(first))
        .expect("pause");
    harness
        .trap(
            second,
            TRAP_HARDWARE_BREAKPOINT,
            debug_registers::STATUS_IDLE | 0b1,
        )
        .expect("unchanged store during the pause");
    assert!(
        !harness
            .trace()
            .take_actions()
            .contains(&format!("continue {second} None")),
        "a thread stopped by the pause stays stopped"
    );
    harness.settle_requested_stops();
    assert_eq!(harness.public_reason(), Some(StopReason::Pause));
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert_eq!(inferior.threads[&second].reason, None);
    assert!(inferior.threads[&second].debugger_stop_pending);
}

/// Puts the harness's first thread in the middle of stepping over a user
/// breakpoint at 0x40 while its sibling waits stopped, as a continue does.
fn repairing_harness() -> WatchHarness {
    repairing_harness_of(0)
}

/// Puts one of two threads in the middle of stepping over a user breakpoint
/// at 0x40 while the other waits stopped, as a continue does.
fn repairing_harness_of(repairing: usize) -> WatchHarness {
    let mut harness = watch_harness(2);
    harness
        .edit(|reply| Edit::AddBreakpoint {
            spec: address_breakpoint(0x40),
            options: Box::default(),
            reply,
        })
        .try_recv()
        .expect("reply")
        .expect("added");
    harness.start_continue();
    let (repairing, waiting) = (harness.threads[repairing], harness.threads[1 - repairing]);
    let inferior = harness.controller.inferior.as_mut().expect("inferior");
    inferior.repairs = VecDeque::from([RepairGroup {
        address: VirtualAddress::new(0x40),
        remaining: VecDeque::new(),
        current: Some(repairing),
        site_removed: true,
    }]);
    inferior
        .breakpoints
        .get_mut(&VirtualAddress::new(0x40))
        .expect("site")
        .installed = false;
    let thread = inferior.thread_mut(repairing).expect("thread");
    thread.stopped_at_breakpoint = Some(VirtualAddress::new(0x40));
    thread.expected = ExpectedStop::BreakpointRepair {
        address: VirtualAddress::new(0x40),
    };
    inferior.thread_mut(waiting).expect("thread").state = NativeThreadState::Stopped;
    harness.trace().take_actions();
    harness.published();
    harness
}

fn deliver(harness: &mut WatchHarness, pid: Pid, signal: Signal) {
    harness.trace().siginfo.borrow_mut().insert(
        pid,
        SignalMetadata {
            code: libc::SI_USER,
            sender: Some(1),
            fault_address: None,
        },
    );
    harness
        .controller
        .process_wait(WaitEvent::Stopped(pid, signal))
        .expect("signal stop");
}

#[test]
fn a_quiet_signal_during_a_repair_runs_its_handler_before_the_repair() {
    let mut harness = repairing_harness();
    let (repairing, waiting) = (harness.threads[0], harness.threads[1]);
    let alarm = Signal::new(libc::SIGALRM).expect("SIGALRM");

    deliver(&mut harness, repairing, alarm);
    // The site is restored while the handler runs alone, so the waiting
    // sibling cannot run past it.
    assert_eq!(
        harness.trace().take_actions(),
        [
            "reinstall_site 0x40".to_owned(),
            format!("continue {repairing} Some({alarm:?})"),
        ]
    );
    assert_eq!(
        harness.thread(repairing).awaiting_breakpoint,
        Some(VirtualAddress::new(0x40))
    );
    assert_eq!(harness.thread(waiting).state, NativeThreadState::Stopped);

    // Returning from the handler re-traps at the site, which the thread
    // now steps over before its sibling resumes.
    harness.hit_at(repairing, 0x40).expect("re-trap");
    assert_eq!(
        harness.trace().take_actions(),
        [
            format!("set_registers {repairing} rip=0x40"),
            "remove_site 0x40".to_owned(),
            format!("step {repairing}"),
        ]
    );
    harness
        .trap(repairing, libc::TRAP_TRACE, debug_registers::STATUS_IDLE)
        .expect("repair step");
    assert_eq!(
        harness.trace().take_actions(),
        [
            "reinstall_site 0x40".to_owned(),
            format!("continue {repairing} None"),
            format!("continue {waiting} None"),
        ]
    );
    assert!(
        !harness
            .published()
            .iter()
            .any(|event| matches!(event, DebuggerEvent::InferiorStopped { .. }))
    );
}

#[test]
fn a_discarded_signal_during_a_repair_repeats_the_repair_step() {
    let mut harness = repairing_harness();
    let repairing = harness.threads[0];
    let alarm = Signal::new(libc::SIGALRM).expect("SIGALRM");
    harness.controller.signals.set(
        alarm,
        SignalPolicy {
            stop: false,
            print: true,
            pass: false,
        },
    );

    deliver(&mut harness, repairing, alarm);
    assert_eq!(
        harness.trace().take_actions(),
        [format!("step {repairing}")]
    );
    let published = harness.published();
    assert!(
        matches!(
            published.as_slice(),
            [DebuggerEvent::SignalReceived { thread_id, exception, .. }]
                if thread_id.get() == u64::try_from(repairing.as_raw()).expect("pid")
                    && exception.code == alarm.code()
        ),
        "{published:?}"
    );
    assert!(harness.thread(repairing).pending_signal.is_none());
}

#[test]
fn threads_ending_before_their_announcement_never_become_live() {
    let mut harness = watch_harness(2);
    let (leader, sibling) = (harness.threads[0], harness.threads[1]);
    let tgid = harness.controller.inferior.as_ref().expect("inferior").tgid;
    harness.start_continue();
    harness.trace().take_actions();
    let unannounced = Pid::from_raw(5009);

    // A sibling's exit_group ends a thread its creator has not announced.
    harness
        .controller
        .process_wait(WaitEvent::PtraceEvent(
            unannounced,
            Signal::SIGTRAP,
            libc::PTRACE_EVENT_EXIT,
        ))
        .expect("an unannounced thread's exit event");
    harness
        .controller
        .process_wait(WaitEvent::Exited(unannounced, 0))
        .expect("an unannounced thread's exit");
    *harness.trace().clone.borrow_mut() = Some((unannounced, tgid));
    harness
        .controller
        .process_wait(WaitEvent::PtraceEvent(
            leader,
            Signal::SIGTRAP,
            libc::PTRACE_EVENT_CLONE,
        ))
        .expect("the late announcement");
    assert!(
        !harness
            .controller
            .inferior
            .as_ref()
            .expect("inferior")
            .threads
            .contains_key(&unannounced),
        "an ended thread became live"
    );
    assert_eq!(
        harness.trace().take_actions(),
        [
            format!("continue {unannounced} None"),
            format!("continue {leader} None"),
        ]
    );

    // A thread whose exit event the kernel released early is no failure.
    harness.trace().vanished.borrow_mut().insert(sibling);
    harness
        .controller
        .process_wait(WaitEvent::PtraceEvent(
            sibling,
            Signal::SIGTRAP,
            libc::PTRACE_EVENT_EXIT,
        ))
        .expect("a released exit event");

    // The leader's exit is reported once every thread is gone, even one
    // whose own exit was never seen.
    harness.published();
    harness
        .controller
        .process_wait(WaitEvent::Exited(leader, 0))
        .expect("the leader's exit");
    assert!(harness.controller.inferior.is_none());
    assert!(harness.published().iter().any(|event| matches!(
        event,
        DebuggerEvent::InferiorExited {
            status: ExitStatus::Code(0),
            ..
        }
    )));
}

/// Call-frame information whose rules read a stack slot no read reaches,
/// as a corrupted stack's do.
struct LostUnwindInfo;

impl UnwindInfo for LostUnwindInfo {
    fn cfa(
        &self,
        _address: ImageAddress,
        _registers: &RegisterFile,
        _memory: &mut dyn MemoryReader,
    ) -> std::result::Result<VirtualAddress, UnwindTermination> {
        Err(UnwindTermination::MemoryReadFailed {
            address: VirtualAddress::new(0x7ff8),
        })
    }

    fn unwind(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<crate::unwind::UnwindStep, UnwindTermination> {
        Err(self
            .cfa(address, registers, memory)
            .expect_err("no rule resolves"))
    }
}

/// A thread stepping over a source line whose plan site at 0x30 traps where
/// its frame can no longer be unwound.
fn lost_frame_harness() -> WatchHarness {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    let site = VirtualAddress::new(0x30);
    let execution = ExecutionId::new(2);
    let controller = &mut harness.controller;
    controller.unwind_info = Arc::new(LostUnwindInfo);
    let inferior = controller.inferior.as_mut().expect("inferior");
    controller
        .ptrace
        .install_breakpoint(
            pid,
            &mut inferior.breakpoints,
            site,
            BreakpointOwner::Plan(execution),
        )
        .expect("plan site");
    inferior
        .plan_sites
        .insert(execution, BTreeSet::from([site]));
    inferior.public_stop = None;
    inferior.active = Some(ActiveExecution {
        id: execution,
        kind: ActiveKind::Step {
            thread: pid,
            kind: StepKind::OverSource,
            start: Box::new(StepStart {
                source: Some(SourceLocation {
                    file: crate::SourceFileId::new(0),
                    line: crate::LineNumber::new(1).expect("nonzero line"),
                    column: None,
                }),
                code_instance: Some(CodeInstanceId::new(0)),
                physical_instance: Some(CodeInstanceId::new(0)),
                activation: Some(VirtualAddress::new(0x7000)),
                plan_addresses: BTreeSet::from([site]),
                epilogue_traversal: None,
                return_traversal: None,
                signal_guard: None,
                call_return: None,
            }),
            progress_owed: false,
        },
        scope: ResumeScope::Thread(debug_thread_id(pid)),
        resume_threads: BTreeSet::from([pid]),
    });
    let thread = harness.thread(pid);
    thread.state = NativeThreadState::Running;
    thread.reason = None;
    harness.trace().take_actions();

    harness
}

/// Call-frame information whose one rule reads the stack, as a real
/// frame's does, so unwinding fails only when that read does.
struct StackReadingUnwindInfo;

impl StackReadingUnwindInfo {
    const SLOT: VirtualAddress = VirtualAddress::new(0x7ff8);
}

impl UnwindInfo for StackReadingUnwindInfo {
    fn cfa(
        &self,
        _address: ImageAddress,
        _registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<VirtualAddress, UnwindTermination> {
        memory
            .read_u64(Self::SLOT)
            .map(|_| VirtualAddress::new(0x8000))
            .ok_or(UnwindTermination::MemoryReadFailed {
                address: Self::SLOT,
            })
    }

    fn unwind(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<crate::unwind::UnwindStep, UnwindTermination> {
        self.cfa(address, registers, memory)?;
        Err(UnwindTermination::Complete)
    }
}

#[test]
fn a_step_whose_thread_sigkill_ends_as_it_unwinds_publishes_no_lost_frame() {
    let mut harness = lost_frame_harness();
    let pid = harness.threads[0];
    // Something outside the debugger kills the program after the hit is
    // classified, as the step reads its frame from the stack.
    harness.controller.unwind_info = Arc::new(StackReadingUnwindInfo);
    *harness.trace().kill_at_read.borrow_mut() = Some((pid, StackReadingUnwindInfo::SLOT.get()));
    harness
        .trace()
        .program_counters
        .borrow_mut()
        .insert(pid, 0x31);
    harness.trace().siginfo.borrow_mut().insert(
        pid,
        SignalMetadata {
            code: libc::SI_KERNEL,
            sender: None,
            fault_address: None,
        },
    );
    assert!(
        harness
            .controller
            .handle_wait(WaitEvent::Stopped(pid, Signal::SIGTRAP))
    );
    assert!(
        harness.trace().kill_at_read.borrow().is_none(),
        "the step read its frame"
    );
    assert_eq!(harness.public_reason(), None, "no lost frame is published");
    assert_eq!(harness.thread(pid).state, NativeThreadState::Running);
}

#[test]
fn a_step_that_loses_its_frame_stops_explicitly_and_leaves_the_program_alone() {
    let mut harness = lost_frame_harness();
    let pid = harness.threads[0];

    // The step's plan site traps, but where the step's frame went can no
    // longer be told.
    harness.hit_at(pid, 0x30).expect("the hit is handled");

    match harness.public_reason() {
        Some(StopReason::StepIncomplete { kind, description }) => {
            assert_eq!(kind, StepKind::OverSource);
            assert!(description.contains("caller"), "{description}");
        }
        other => panic!("expected an incomplete step, got {other:?}"),
    }
    let actions = harness.trace().take_actions();
    assert!(
        !actions.iter().any(|action| action.starts_with("kill")),
        "the program must not be killed: {actions:?}"
    );
    assert!(
        harness
            .controller
            .inferior
            .as_ref()
            .expect("inferior")
            .breakpoints
            .values()
            .all(|site| !site.installed),
        "the step's plan sites are removed"
    );
}

#[test]
fn an_edit_waiting_for_an_internal_stop_is_answered_when_the_process_ends_first() {
    let mut harness = watch_harness(2);
    harness.start_continue();
    harness.published();
    let mut added = harness.edit(|reply| Edit::AddBreakpoint {
        spec: address_breakpoint(0x40),
        options: Box::default(),
        reply,
    });
    assert!(added.try_recv().is_err(), "the edit waits for every thread");

    // The program exits before either thread stops for the edit.
    for pid in [harness.threads[1], harness.threads[0]] {
        harness
            .controller
            .process_wait(WaitEvent::Exited(pid, 0))
            .expect("an exit is handled");
    }
    assert!(harness.controller.inferior.is_none());
    let breakpoint = added
        .try_recv()
        .expect("the edit is answered")
        .expect("a breakpoint outlives the process it was set in");
    assert_eq!(
        harness
            .controller
            .breakpoints
            .iter()
            .map(|breakpoint| breakpoint.id)
            .collect::<Vec<_>>(),
        [breakpoint.id]
    );
}

#[test]
fn a_step_whose_thread_exits_while_an_edit_drops_the_other_reason_ends_in_its_exit() {
    let mut harness = watch_harness(2);
    let (other, stepping) = (harness.threads[0], harness.threads[1]);
    let breakpoint = harness
        .edit(|reply| Edit::AddBreakpoint {
            spec: address_breakpoint(0x40),
            options: Box::default(),
            reply,
        })
        .try_recv()
        .expect("reply")
        .expect("added at the stop");
    harness.start_continue();
    let inferior = harness.controller.inferior.as_mut().expect("inferior");
    inferior.active.as_mut().expect("an execution").kind = ActiveKind::Step {
        thread: stepping,
        kind: StepKind::OverSource,
        start: Box::new(StepStart {
            source: None,
            code_instance: None,
            physical_instance: None,
            activation: Some(VirtualAddress::new(0x7000)),
            plan_addresses: BTreeSet::new(),
            epilogue_traversal: None,
            return_traversal: None,
            signal_guard: None,
            call_return: None,
        }),
        progress_owed: false,
    };
    harness.published();

    // The edit stops every thread; meanwhile the other thread hits the
    // breakpoint being removed, and the stepping thread exits.
    let mut removed = harness.edit(|reply| Edit::RemoveBreakpoint {
        id: breakpoint.id,
        reply,
    });
    harness.hit_at(other, 0x40).expect("breakpoint trap");
    harness
        .controller
        .process_wait(WaitEvent::Exited(stepping, 0))
        .expect("the stepping thread's exit is handled");
    removed.try_recv().expect("reply").expect("removed");

    // The hit died with its breakpoint, but the step cannot resume without
    // its thread: it ends in that thread's exit.
    assert_eq!(
        harness.public_reason(),
        Some(StopReason::ThreadExited {
            thread_id: debug_thread_id(stepping),
            status: ExitStatus::Code(0),
        })
    );
    assert!(
        !harness
            .trace()
            .take_actions()
            .iter()
            .any(|action| action.starts_with("kill")),
        "the program must not be killed"
    );
}

/// A stack of two frames: the innermost at 0x30 with its CFA at 0x1000,
/// whose caller at 0x50 is the outermost.
struct OutermostCallerUnwindInfo;

impl UnwindInfo for OutermostCallerUnwindInfo {
    fn cfa(
        &self,
        address: ImageAddress,
        _registers: &RegisterFile,
        _memory: &mut dyn MemoryReader,
    ) -> std::result::Result<VirtualAddress, UnwindTermination> {
        match address.get() {
            0x30 => Ok(VirtualAddress::new(0x1000)),
            0x4f => Ok(VirtualAddress::new(0x2000)),
            _ => Err(UnwindTermination::NoUnwindInfo {
                address: VirtualAddress::new(address.get()),
            }),
        }
    }

    fn unwind(
        &self,
        address: ImageAddress,
        _registers: &RegisterFile,
        _memory: &mut dyn MemoryReader,
    ) -> std::result::Result<crate::unwind::UnwindStep, UnwindTermination> {
        match address.get() {
            0x30 => Ok(crate::unwind::UnwindStep {
                registers: RegisterFile::new([(16, 0x50), (7, 0x1000)]),
                cfa: VirtualAddress::new(0x1000),
                signal_frame: false,
            }),
            _ => Err(UnwindTermination::Complete),
        }
    }
}

#[test]
fn an_activation_missing_from_a_wholly_unwound_stack_has_returned() {
    let mut harness = watch_harness(1);
    harness.controller.unwind_info = Arc::new(OutermostCallerUnwindInfo);
    let mut registers = harness
        .trace()
        .registers(harness.threads[0])
        .expect("registers");
    registers.rip = 0x30;
    registers.rsp = 0x0ff8;
    // Above every frame of this stack: another stack's activation, as a
    // raw-cloned thread computes from its creator's frame pointer.
    let elsewhere = VirtualAddress::new(0x9000);
    assert!(
        harness
            .controller
            .location_for_activation(harness.threads[0], &registers, elsewhere)
            .expect("a complete stack is evidence")
            .is_none()
    );
}
