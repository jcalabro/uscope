use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::marker::PhantomData;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle, ThreadId};

use nix::errno::Errno;
use nix::libc;
use nix::sys::ptrace::{self, Options};
use nix::sys::signal::{self, Signal as NixSignal};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use object::{Object, ObjectSegment};
use tokio::sync::{broadcast, mpsc};

use super::ControllerMessage;
use crate::debug_info::{UnwindInfo, VariableInfo, VariableRegister, VariableRuntime};
use crate::model::FrameMetadata;
use crate::protocol::{
    Breakpoint, BreakpointId, BreakpointSpec, DebuggerEvent, ExceptionDisposition, ExceptionInfo,
    ExecutionId, ExitStatus, FramePresentation, InferiorState, PresentedFrame, ProcessId, Reply,
    Request, ResolvedBreakpointLocation, ResumeScope, StateSnapshot, StepKind, StopId, StopReason,
    ThreadSnapshot, ThreadState as ObservableThreadState, VariableQuery,
};
use crate::unwind::{
    CallerProvider, CallerResult, DEFAULT_MAX_FRAMES, FrameContext, MemoryReader, RegisterFile,
    collect_backtrace,
};
use crate::{
    Backtrace, BreakpointLocation, CodeInstanceId, CodeInstanceKind, Error, ExecutionLocation,
    FrameKind, ImageAddress, ImageLocation, InlineFrameLookup, LoadedModule, ModuleImage,
    RegisterDescriptor, RegisterId, RegisterRole, RegisterSnapshot, RegisterValue, Result,
    SourceLocation, StackFrame, ThreadId as DebugThreadId, UnwindTermination, VariableSnapshot,
    VariableUnavailableReason, VirtualAddress,
};

const CONTROLLER_THREAD_NAME: &str = "uscope-controller";
const WAITER_THREAD_NAME: &str = "uscope-waitpid";
const BREAKPOINT_OPCODE: u8 = 0xcc;
const TRAP_UNKNOWN: i32 = 5;
const MAX_LOGICAL_MEMORY_READ: usize = 1024 * 1024;

static LINUX_SESSION_ACTIVE: AtomicBool = AtomicBool::new(false);

pub type WaitEvent = WaitStatus;

fn backend_error(error: LinuxError) -> Error {
    Error::backend(error)
}

fn process_id(pid: Pid) -> ProcessId {
    ProcessId::new(u64::from(pid.as_raw().unsigned_abs()))
}

fn debug_thread_id(pid: Pid) -> DebugThreadId {
    DebugThreadId::new(u64::from(pid.as_raw().unsigned_abs()))
}

fn debug_pid(thread: DebugThreadId) -> Pid {
    Pid::from_raw(i32::try_from(thread.get()).expect("Linux TID fits i32"))
}

fn exception_info(signal: NixSignal) -> ExceptionInfo {
    ExceptionInfo::new(u64::from(signal as u32), signal.to_string())
}

fn pending_exception_info(pending: PendingSignal) -> ExceptionInfo {
    let sender = pending
        .sender
        .map_or_else(|| "unavailable".to_owned(), |sender| sender.to_string());
    ExceptionInfo::new(
        u64::from(pending.signal as u32),
        format!(
            "{} (si_code {}, sender {})",
            pending.signal, pending.code, sender
        ),
    )
}

struct SessionLease {
    owns_global_lease: bool,
}

impl SessionLease {
    fn acquire() -> Result<Self> {
        LINUX_SESSION_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| backend_error(LinuxError::SessionActive))?;
        Ok(Self {
            owns_global_lease: true,
        })
    }

    #[cfg(test)]
    const fn detached() -> Self {
        Self {
            owns_global_lease: false,
        }
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        if !self.owns_global_lease {
            return;
        }
        assert!(
            LINUX_SESSION_ACTIVE.swap(false, Ordering::AcqRel),
            "Linux tracing session lease was active"
        );
    }
}

struct BreakpointSite {
    original_byte: u8,
    installed: bool,
    owners: BTreeSet<BreakpointOwner>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum BreakpointOwner {
    User(BreakpointId),
    Plan(ExecutionId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeThreadState {
    Starting,
    Running,
    StopRequested { barrier: u64 },
    Stopped,
    Exiting,
}

#[derive(Debug, Clone)]
enum ExpectedStop {
    InitialExec,
    None,
    BreakpointRepair { address: VirtualAddress },
    AwaitBreakpoint { address: VirtualAddress },
    UserStep { kind: StepKind },
}

struct TraceThread {
    state: NativeThreadState,
    expected: ExpectedStop,
    pending_signal: Option<PendingSignal>,
    reason: Option<StopReason>,
    stopped_at_breakpoint: Option<VirtualAddress>,
    awaiting_breakpoint: Option<VirtualAddress>,
    debugger_stop_pending: bool,
}

impl TraceThread {
    const fn starting(expected: ExpectedStop) -> Self {
        Self {
            state: NativeThreadState::Starting,
            expected,
            pending_signal: None,
            reason: None,
            stopped_at_breakpoint: None,
            awaiting_breakpoint: None,
            debugger_stop_pending: false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PendingSignal {
    signal: NixSignal,
    code: i32,
    sender: Option<i32>,
}

#[derive(Debug, Clone, Copy)]
struct SignalMetadata {
    code: i32,
    sender: Option<i32>,
}

#[derive(Debug)]
struct RawStopRecord {
    status: String,
    siginfo: std::result::Result<SignalMetadata, Errno>,
}

#[derive(Debug)]
enum ClassifiedStop {
    ThreadStart,
    SignalDelivery(PendingSignal),
    GroupStop(NixSignal),
    Breakpoint(VirtualAddress),
    Trace,
    DebuggerRequested,
    Unclassifiable(RawStopRecord),
}

#[derive(Debug, Clone)]
struct StepStart {
    source: Option<SourceLocation>,
    code_instance: Option<CodeInstanceId>,
    activation: Option<VirtualAddress>,
    plan_addresses: BTreeSet<VirtualAddress>,
}

#[derive(Debug, Clone)]
enum ActiveKind {
    Launch,
    Continue,
    Step {
        thread: Pid,
        kind: StepKind,
        start: StepStart,
    },
}

struct ActiveExecution {
    id: ExecutionId,
    kind: ActiveKind,
    scope: ResumeScope,
    resume_threads: BTreeSet<Pid>,
}

struct RepairGroup {
    address: VirtualAddress,
    remaining: VecDeque<Pid>,
    current: Option<Pid>,
    site_removed: bool,
}

struct StopBarrier {
    execution: Option<ExecutionId>,
    triggering_thread: Pid,
    reason: StopReason,
}

struct PublicStop {
    id: StopId,
    triggering_thread: Pid,
    reason: StopReason,
    presentations: BTreeMap<Pid, FramePresentation>,
}

struct Inferior {
    tgid: Pid,
    loaded_module: LoadedModule,
    breakpoints: BTreeMap<VirtualAddress, BreakpointSite>,
    threads: BTreeMap<Pid, TraceThread>,
    retired_threads: BTreeSet<Pid>,
    unowned_stops: BTreeMap<Pid, WaitStatus>,
    waiter: Option<JoinHandle<()>>,
    active: Option<ActiveExecution>,
    repairs: VecDeque<RepairGroup>,
    barrier: Option<StopBarrier>,
    public_stop: Option<PublicStop>,
    selected_thread: Option<Pid>,
    next_execution: u64,
    next_stop: u64,
    next_barrier: u64,
    exec_unsupported: bool,
}

#[derive(Debug, thiserror::Error)]
enum LinuxError {
    #[error("system tracing operation failed: {0}")]
    System(#[from] nix::Error),
    #[error("failed to parse executable object: {0}")]
    Object(#[from] object::Error),
    #[error("unexpected wait status: {0}")]
    UnexpectedWait(String),
    #[error("could not determine load bias for {0}")]
    LoadBias(PathBuf),
    #[error("another Linux tracing session is already active in this process")]
    SessionActive,
    #[error("unsupported clone created a different thread group {0}")]
    UnsupportedClone(i32),
    #[error("floating-point register reads are unsupported by this tracing effect")]
    UnsupportedFloatingRegisters,
    #[error("the inferior replaced its executable image; loading the new image is not supported")]
    UnsupportedExec,
    #[error("could not determine the caller frame for step out: {0:?}")]
    CallerUnavailable(UnwindTermination),
    #[error("breakpoint site {0:?} was not found")]
    BreakpointSiteMissing(VirtualAddress),
    #[error("breakpoint site {0:?} did not have the expected owner")]
    BreakpointOwnerMissing(VirtualAddress),
    #[error("logical breakpoint identifiers were exhausted")]
    BreakpointIdExhausted,
    #[error("breakpoint installation failed ({cause}) and rollback also failed ({recovery})")]
    BreakpointInstallRecovery { cause: String, recovery: String },
    #[error("breakpoint removal failed ({cause}) and rollback also failed ({recovery})")]
    BreakpointRemoveRecovery { cause: String, recovery: String },
    #[error("resume failed ({cause}) and recovery also failed ({recovery})")]
    ResumeRecovery { cause: String, recovery: String },
    #[error("logical memory read of {size} bytes exceeds the {maximum}-byte limit")]
    MemoryReadTooLarge { size: usize, maximum: usize },
}

struct Controller<P: LinuxTraceOps> {
    _lease: SessionLease,
    executable: Arc<PathBuf>,
    module_image: Arc<ModuleImage>,
    unwind_info: Arc<dyn UnwindInfo>,
    variable_info: Arc<dyn VariableInfo>,
    messages: mpsc::Receiver<ControllerMessage>,
    message_sender: mpsc::Sender<ControllerMessage>,
    events: broadcast::Sender<DebuggerEvent>,
    ptrace: P,
    inferior: Option<Inferior>,
    breakpoints: Vec<Breakpoint>,
    next_breakpoint_id: u64,
    launch_reply: Option<Reply<ExecutionId>>,
    shutdown_reply: Option<Reply<()>>,
    revision: u64,
}

struct ControllerChannels {
    messages: mpsc::Receiver<ControllerMessage>,
    message_sender: mpsc::Sender<ControllerMessage>,
    events: broadcast::Sender<DebuggerEvent>,
}

pub fn spawn_controller(
    executable: Arc<PathBuf>,
    module_image: Arc<ModuleImage>,
    unwind_info: Arc<dyn UnwindInfo>,
    variable_info: Arc<dyn VariableInfo>,
    message_sender: mpsc::Sender<ControllerMessage>,
    messages: mpsc::Receiver<ControllerMessage>,
    events: broadcast::Sender<DebuggerEvent>,
) -> Result<JoinHandle<()>> {
    let lease = SessionLease::acquire()?;

    Ok(thread::Builder::new()
        .name(CONTROLLER_THREAD_NAME.into())
        .spawn(move || {
            Controller::new(
                lease,
                executable,
                module_image,
                unwind_info,
                variable_info,
                ControllerChannels {
                    messages,
                    message_sender,
                    events,
                },
                LinuxPtrace::new(),
            )
            .run();
        })?)
}

impl<P: LinuxTraceOps> Controller<P> {
    fn new(
        lease: SessionLease,
        executable: Arc<PathBuf>,
        module_image: Arc<ModuleImage>,
        unwind_info: Arc<dyn UnwindInfo>,
        variable_info: Arc<dyn VariableInfo>,
        channels: ControllerChannels,
        ptrace: P,
    ) -> Self {
        Self {
            _lease: lease,
            executable,
            module_image,
            unwind_info,
            variable_info,
            messages: channels.messages,
            message_sender: channels.message_sender,
            events: channels.events,
            ptrace,
            inferior: None,
            breakpoints: Vec::new(),
            next_breakpoint_id: 1,
            launch_reply: None,
            shutdown_reply: None,
            revision: 0,
        }
    }

    fn run(mut self) {
        while let Some(message) = self.messages.blocking_recv() {
            let keep_running = match message {
                ControllerMessage::Request(request) => self.handle_request(request),
                ControllerMessage::Wait(status) => self.handle_wait(status),
            };

            if !keep_running {
                return;
            }
        }

        self.begin_shutdown(None);
        while self.inferior.is_some() {
            let Some(ControllerMessage::Wait(status)) = self.messages.blocking_recv() else {
                break;
            };
            if !self.handle_wait(status) {
                break;
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the exhaustive request dispatcher keeps protocol routing in one place"
    )]
    fn handle_request(&mut self, request: Request) -> bool {
        match request {
            Request::AddBreakpoint { spec, reply } => {
                let _ = reply.send(self.add_breakpoint(spec));
            }
            Request::RemoveBreakpoint { id, reply } => {
                let _ = reply.send(self.remove_breakpoint(id));
            }
            Request::RemoveAllBreakpoints { reply } => {
                let _ = reply.send(self.remove_all_breakpoints());
            }
            Request::Launch { reply } => self.launch(reply),
            Request::Continue {
                process_id,
                stop_id,
                scope,
                exception,
                reply,
            } => self.resume(process_id, stop_id, scope, exception, reply),
            Request::Step {
                process_id,
                stop_id,
                thread_id,
                kind,
                exception,
                reply,
            } => self.step(
                process_id,
                stop_id,
                debug_pid(thread_id),
                kind,
                exception,
                reply,
            ),
            Request::Pause { process_id, reply } => self.pause(process_id, reply),
            Request::ReadWord {
                process_id,
                stop_id,
                address,
                reply,
            } => {
                let _ = reply.send(self.read_word(process_id, stop_id, address));
            }
            Request::WriteWord {
                process_id,
                stop_id,
                address,
                value,
                reply,
            } => {
                let result = self.write_word(process_id, stop_id, address, value);
                let _ = reply.send(result);
            }
            Request::LoadedModule { reply } => {
                let _ = reply.send(self.loaded_module());
            }
            Request::StoppedLocation {
                stop_id,
                thread_id,
                reply,
            } => {
                let _ = reply.send(self.stopped_location(stop_id, debug_pid(thread_id)));
            }
            Request::Snapshot { reply } => {
                let _ = reply.send(Ok(self.snapshot()));
            }
            Request::Backtrace {
                stop_id,
                thread_id,
                reply,
            } => {
                let _ = reply.send(self.backtrace(stop_id, debug_pid(thread_id)));
            }
            Request::Registers {
                stop_id,
                thread_id,
                reply,
            } => {
                let _ = reply.send(self.registers(stop_id, debug_pid(thread_id)));
            }
            Request::Variables {
                query,
                stop_id,
                thread_id,
                reply,
            } => {
                let _ = reply.send(self.variables(stop_id, debug_pid(thread_id), &query));
            }
            Request::SelectThread {
                stop_id,
                thread_id,
                reply,
            } => {
                let result = self.select_thread(stop_id, debug_pid(thread_id));
                let _ = reply.send(result);
            }
            Request::Shutdown { reply } => {
                self.begin_shutdown(Some(reply));
                return self.inferior.is_some();
            }
        }

        true
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    fn handle_wait(&mut self, status: WaitStatus) -> bool {
        if self.shutdown_reply.is_some() {
            return self.handle_shutdown_wait(status);
        }

        if let Err(error) = self.process_wait(status) {
            self.fail_inferior(error);
        }

        true
    }

    fn process_wait(&mut self, status: WaitStatus) -> Result<()> {
        let pid = wait_status_pid(&status)
            .ok_or_else(|| backend_error(LinuxError::UnexpectedWait(format!("{status:?}"))))?;
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let known = inferior.threads.contains_key(&pid);
        let retired = inferior.retired_threads.contains(&pid);

        if !known {
            if retired && matches!(status, WaitStatus::Exited(..) | WaitStatus::Signaled(..)) {
                self.inferior
                    .as_mut()
                    .expect("inferior exists")
                    .retired_threads
                    .remove(&pid);
                return Ok(());
            }
            if matches!(status, WaitStatus::Stopped(..)) {
                self.inferior
                    .as_mut()
                    .expect("inferior exists")
                    .unowned_stops
                    .insert(pid, status);
                return Ok(());
            }
            return Err(backend_error(LinuxError::UnexpectedWait(format!(
                "unowned {status:?}"
            ))));
        }

        match status {
            WaitStatus::Exited(pid, code) => {
                self.handle_terminal(pid, ExitStatus::Code(i64::from(code)))
            }
            WaitStatus::Signaled(pid, signal, _) => {
                self.handle_terminal(pid, ExitStatus::Terminated(exception_info(signal)))
            }
            WaitStatus::PtraceEvent(pid, _, event) => self.handle_ptrace_event(pid, event),
            WaitStatus::PtraceSyscall(pid) => self.handle_classified_stop(
                pid,
                ClassifiedStop::Unclassifiable(RawStopRecord {
                    status: "ptrace syscall stop while syscall tracing is unsupported".to_owned(),
                    siginfo: Err(Errno::EINVAL),
                }),
            ),
            WaitStatus::Stopped(pid, signal) => {
                let initial = self
                    .inferior
                    .as_ref()
                    .and_then(|inferior| inferior.threads.get(&pid))
                    .is_some_and(|thread| matches!(thread.expected, ExpectedStop::InitialExec));
                if initial && signal == NixSignal::SIGTRAP {
                    self.handle_initial_stop(pid)
                } else {
                    let stop = self.classify_stop(pid, signal);
                    self.handle_classified_stop(pid, stop)
                }
            }
            other => Err(backend_error(LinuxError::UnexpectedWait(format!(
                "{other:?}"
            )))),
        }
    }

    fn add_breakpoint(&mut self, spec: BreakpointSpec) -> Result<Breakpoint> {
        if let Some(existing) = self
            .breakpoints
            .iter()
            .find(|breakpoint| breakpoint.spec == spec)
        {
            return Ok(existing.clone());
        }

        let id = BreakpointId::new(self.next_breakpoint_id);
        let next_id = self
            .next_breakpoint_id
            .checked_add(1)
            .ok_or_else(|| backend_error(LinuxError::BreakpointIdExhausted))?;
        let breakpoint = self.resolve_breakpoint(id, spec)?;

        if let Some(inferior) = self.inferior.as_mut() {
            validate_public_stop(inferior, inferior.public_stop.as_ref().map(|stop| stop.id))?;
            install_logical_breakpoint(&self.ptrace, inferior, &breakpoint)?;
        }

        self.next_breakpoint_id = next_id;
        self.breakpoints.push(breakpoint.clone());
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::BreakpointsChanged {
            revision: self.revision,
        });

        Ok(breakpoint)
    }

    fn resolve_breakpoint(&self, id: BreakpointId, spec: BreakpointSpec) -> Result<Breakpoint> {
        let locations: Arc<[ResolvedBreakpointLocation]> = match &spec {
            BreakpointSpec::Address(address) => Arc::from([ResolvedBreakpointLocation {
                location: BreakpointLocation::Virtual(*address),
                code_instances: Arc::from([]),
            }]),
            BreakpointSpec::Function(name) => self.resolve_function_breakpoint(std::iter::once(
                self.module_image.function_named(name)?,
            ))?,
            BreakpointSpec::FileFunction { path, function } => {
                let source = self.module_image.source_file_matching(path)?;
                let functions = self
                    .module_image
                    .functions()
                    .iter()
                    .filter(|candidate| candidate.name.as_ref() == function)
                    .filter(|candidate| {
                        candidate
                            .declaration
                            .as_ref()
                            .is_some_and(|location| location.file == source.id)
                    })
                    .collect::<Vec<_>>();
                if functions.is_empty() {
                    return Err(Error::FunctionNotFound(function.clone()));
                }
                self.resolve_function_breakpoint(functions)?
            }
            BreakpointSpec::Source { path, line } => {
                let source = self.module_image.source_file_matching(path)?;
                let addresses = self
                    .module_image
                    .statement_addresses(source.id, *line)
                    .collect::<Vec<_>>();
                if addresses.is_empty() {
                    return Err(Error::SourceLineUnavailable {
                        path: path.clone(),
                        line: line.get(),
                    });
                }
                addresses
                    .into_iter()
                    .map(|address| {
                        let code_instances = self
                            .module_image
                            .code_instances()
                            .iter()
                            .filter(|instance| instance.contains(address))
                            .map(|instance| instance.id)
                            .collect::<Vec<_>>()
                            .into();
                        ResolvedBreakpointLocation {
                            location: BreakpointLocation::Image(address),
                            code_instances,
                        }
                    })
                    .collect::<Vec<_>>()
                    .into()
            }
        };

        Ok(Breakpoint {
            id,
            spec,
            locations,
        })
    }

    fn resolve_function_breakpoint<'a>(
        &self,
        functions: impl IntoIterator<Item = &'a crate::FunctionInfo>,
    ) -> Result<Arc<[ResolvedBreakpointLocation]>> {
        let mut instances = Vec::new();
        for function in functions {
            instances.extend(self.module_image.instances_for_function(function.id));
        }
        if instances.is_empty()
            || instances
                .iter()
                .any(|instance| instance.breakpoint_entry.is_none())
        {
            return Err(Error::LocationUnavailable);
        }

        let mut by_address = BTreeMap::<_, Vec<_>>::new();
        for instance in instances {
            let entry = instance.breakpoint_entry.expect("entries were validated");
            by_address
                .entry(entry.address)
                .or_default()
                .push(instance.id);
        }

        Ok(by_address
            .into_iter()
            .map(|(address, code_instances)| ResolvedBreakpointLocation {
                location: BreakpointLocation::Image(address),
                code_instances: code_instances.into(),
            })
            .collect::<Vec<_>>()
            .into())
    }

    fn remove_breakpoint(&mut self, id: BreakpointId) -> Result<Breakpoint> {
        let index = self
            .breakpoints
            .iter()
            .position(|breakpoint| breakpoint.id == id)
            .ok_or(Error::BreakpointNotFound(id.get()))?;
        let breakpoint = self.breakpoints[index].clone();
        if let Some(inferior) = self.inferior.as_mut() {
            validate_public_stop(inferior, None)?;
            remove_logical_breakpoint(&self.ptrace, inferior, &breakpoint)?;
        }
        self.breakpoints.remove(index);
        self.publish_breakpoints_changed();
        Ok(breakpoint)
    }

    fn remove_all_breakpoints(&mut self) -> Result<Arc<[Breakpoint]>> {
        if self.breakpoints.is_empty() {
            return Ok(Arc::from([]));
        }
        if let Some(inferior) = self.inferior.as_mut() {
            validate_public_stop(inferior, None)?;
            let stopped_at = inferior
                .threads
                .iter()
                .map(|(&pid, thread)| (pid, thread.stopped_at_breakpoint))
                .collect::<Vec<_>>();
            let mut removed = Vec::new();
            for breakpoint in &self.breakpoints {
                if let Err(cause) = remove_logical_breakpoint(&self.ptrace, inferior, breakpoint) {
                    for prior in removed.iter().rev() {
                        if let Err(recovery) =
                            install_logical_breakpoint(&self.ptrace, inferior, prior)
                        {
                            return Err(backend_error(LinuxError::BreakpointRemoveRecovery {
                                cause: cause.to_string(),
                                recovery: recovery.to_string(),
                            }));
                        }
                    }
                    for (pid, address) in stopped_at {
                        inferior
                            .threads
                            .get_mut(&pid)
                            .expect("captured thread remains stopped")
                            .stopped_at_breakpoint = address;
                    }
                    return Err(cause);
                }
                removed.push(breakpoint.clone());
            }
        }
        let removed: Arc<[Breakpoint]> = std::mem::take(&mut self.breakpoints).into();
        self.publish_breakpoints_changed();
        Ok(removed)
    }

    fn publish_breakpoints_changed(&mut self) {
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::BreakpointsChanged {
            revision: self.revision,
        });
    }

    fn launch(&mut self, reply: Reply<ExecutionId>) {
        if self.inferior.is_some() || self.launch_reply.is_some() {
            let _ = reply.send(Err(Error::AlreadyRunning));
            return;
        }

        match self.ptrace.spawn(&self.executable) {
            Ok(pid) => {
                let waiter = match self.ptrace.spawn_waiter(self.message_sender.clone()) {
                    Ok(waiter) => waiter,
                    Err(error) => {
                        let _ = self.ptrace.kill(pid, NixSignal::SIGKILL);
                        let _ = self.ptrace.reap(pid);
                        let _ = reply.send(Err(error));
                        return;
                    }
                };
                let process_id = process_id(pid);
                let execution_id = ExecutionId::new(1);
                let mut threads = BTreeMap::new();
                threads.insert(pid, TraceThread::starting(ExpectedStop::InitialExec));

                self.inferior = Some(Inferior {
                    tgid: pid,
                    loaded_module: LoadedModule::main(self.module_image.id(), 0),
                    breakpoints: BTreeMap::new(),
                    threads,
                    retired_threads: BTreeSet::new(),
                    unowned_stops: BTreeMap::new(),
                    waiter: Some(waiter),
                    active: Some(ActiveExecution {
                        id: execution_id,
                        kind: ActiveKind::Launch,
                        scope: ResumeScope::Process(process_id),
                        resume_threads: BTreeSet::from([pid]),
                    }),
                    repairs: VecDeque::new(),
                    barrier: None,
                    public_stop: None,
                    selected_thread: None,
                    next_execution: 1,
                    next_stop: 0,
                    next_barrier: 0,
                    exec_unsupported: false,
                });
                self.launch_reply = Some(reply);
                self.bump_revision();
                let _ = self.events.send(DebuggerEvent::InferiorLaunched {
                    revision: self.revision,
                    process_id,
                    execution_id,
                });
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn handle_initial_stop(&mut self, pid: Pid) -> Result<()> {
        self.ptrace.set_options(pid)?;
        let load_bias = self.ptrace.load_bias(pid, &self.executable)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.loaded_module = LoadedModule::main(self.module_image.id(), load_bias);

        for breakpoint in &self.breakpoints {
            install_logical_breakpoint(&self.ptrace, inferior, breakpoint)?;
        }

        self.ptrace.continue_execution(pid, None)?;
        let thread = inferior
            .threads
            .get_mut(&pid)
            .expect("initial thread exists");
        thread.state = NativeThreadState::Running;
        thread.expected = ExpectedStop::None;
        let execution_id = inferior.active.as_ref().expect("launch is active").id;
        let process_id = process_id(inferior.tgid);
        self.bump_revision();
        if let Some(reply) = self.launch_reply.take() {
            let _ = reply.send(Ok(execution_id));
        }
        let _ = self.events.send(DebuggerEvent::InferiorContinued {
            revision: self.revision,
            process_id,
            execution_id,
            resumed: ResumeScope::Process(process_id),
        });
        Ok(())
    }

    fn resume(
        &mut self,
        process_id: ProcessId,
        stop_id: StopId,
        scope: ResumeScope,
        exception: ExceptionDisposition,
        reply: Reply<ExecutionId>,
    ) {
        let result =
            self.begin_execution(process_id, stop_id, scope, ActiveKind::Continue, exception);
        self.reply_execution(result, scope, reply);
    }

    fn step(
        &mut self,
        process_id: ProcessId,
        stop_id: StopId,
        pid: Pid,
        kind: StepKind,
        exception: ExceptionDisposition,
        reply: Reply<ExecutionId>,
    ) {
        let scope = ResumeScope::Thread(debug_thread_id(pid));
        match self.try_virtual_step(process_id, stop_id, pid, kind) {
            Ok(Some(execution)) => {
                let _ = reply.send(Ok(execution));
                return;
            }
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
            Ok(None) => {}
        }
        let result = self.step_start(pid, kind).and_then(|start| {
            self.begin_execution(
                process_id,
                stop_id,
                scope,
                ActiveKind::Step {
                    thread: pid,
                    kind,
                    start,
                },
                exception,
            )
        });
        self.reply_execution(result, scope, reply);
    }

    fn try_virtual_step(
        &mut self,
        requested_process: ProcessId,
        stop_id: StopId,
        pid: Pid,
        kind: StepKind,
    ) -> Result<Option<ExecutionId>> {
        if kind != StepKind::IntoSource {
            return Ok(None);
        }

        let (process_id, execution_id, next_stop_id, presentation) = {
            let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
            validate_process(inferior, requested_process)?;
            validate_public_stop(inferior, Some(stop_id))?;
            validate_stopped_thread(inferior, pid)?;
            let stop = inferior
                .public_stop
                .as_ref()
                .expect("public stop was validated");
            let Some(current) = stop.presentations.get(&pid) else {
                return Ok(None);
            };
            if current.hidden_inline_frames == 0
                || matches!(current.frame, PresentedFrame::Ambiguous(_))
            {
                return Ok(None);
            }

            let image_address = inferior.loaded_module.image_address(current.instruction)?;
            let location = self.module_image.locate(image_address);
            let InlineFrameLookup::Unique(chain) = &location.inline_frames else {
                return Err(Error::AmbiguousInlineFrame);
            };
            let visible = presentation_visible_count(&location, current)?;
            let presentation =
                make_presentation(current.instruction, chain.instances.as_ref(), visible + 1)?;

            (
                process_id(inferior.tgid),
                ExecutionId::new(inferior.next_execution.wrapping_add(1)),
                StopId::new(inferior.next_stop.wrapping_add(1)),
                presentation,
            )
        };

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.next_execution = execution_id.get();
        inferior.next_stop = next_stop_id.get();
        let stop = inferior
            .public_stop
            .as_mut()
            .expect("public stop was validated");
        stop.id = next_stop_id;
        stop.reason = StopReason::Step { kind };
        stop.presentations.insert(pid, presentation);
        inferior
            .threads
            .get_mut(&pid)
            .expect("stopped thread exists")
            .reason = Some(StopReason::Step { kind });
        inferior.selected_thread = Some(pid);

        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::InferiorStopped {
            revision: self.revision,
            process_id,
            execution_id: Some(execution_id),
            stop_id: next_stop_id,
            thread_id: debug_thread_id(pid),
            all_threads_stopped: true,
            reason: StopReason::Step { kind },
        });

        Ok(Some(execution_id))
    }

    fn pause(&mut self, process_id: ProcessId, reply: Reply<ExecutionId>) {
        let result = self.begin_pause(process_id);
        let _ = reply.send(result);
    }

    fn begin_execution(
        &mut self,
        requested_process: ProcessId,
        stop_id: StopId,
        scope: ResumeScope,
        kind: ActiveKind,
        exception: ExceptionDisposition,
    ) -> Result<ExecutionId> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        validate_process(inferior, requested_process)?;
        validate_public_stop(inferior, Some(stop_id))?;
        if inferior
            .public_stop
            .as_ref()
            .is_some_and(|stop| matches!(stop.reason, StopReason::Unclassifiable { .. }))
        {
            return Err(Error::UnclassifiableStop);
        }
        if inferior.exec_unsupported {
            return Err(backend_error(LinuxError::UnsupportedExec));
        }

        let resume_threads = scoped_threads(inferior, scope)?;
        inferior.next_execution = inferior.next_execution.wrapping_add(1);
        let execution_id = ExecutionId::new(inferior.next_execution);
        let owner = BreakpointOwner::Plan(execution_id);
        let mut installed = Vec::new();
        if let ActiveKind::Step { start, .. } = &kind {
            for &address in &start.plan_addresses {
                if let Err(error) = self.ptrace.install_breakpoint(
                    inferior.tgid,
                    &mut inferior.breakpoints,
                    address,
                    owner,
                ) {
                    for address in installed.into_iter().rev() {
                        if let Err(recovery) =
                            remove_breakpoint_owner_from(&self.ptrace, inferior, address, owner)
                        {
                            let _ = self.ptrace.kill(inferior.tgid, NixSignal::SIGKILL);
                            return Err(backend_error(LinuxError::ResumeRecovery {
                                cause: error.to_string(),
                                recovery: recovery.to_string(),
                            }));
                        }
                    }
                    return Err(error);
                }
                installed.push(address);
            }
        }
        let mut suppressed = Vec::new();
        if exception == ExceptionDisposition::Suppress {
            for &pid in &resume_threads {
                let thread = inferior
                    .threads
                    .get_mut(&pid)
                    .expect("scoped thread exists");
                if let Some(pending) = thread.pending_signal.take() {
                    suppressed.push((pid, pending));
                }
            }
        }
        inferior.public_stop = None;
        inferior.active = Some(ActiveExecution {
            id: execution_id,
            kind,
            scope,
            resume_threads,
        });
        inferior.repairs = collect_repairs(inferior);

        if let Err(error) = self.advance_execution() {
            self.restore_unconsumed_signals(&suppressed);
            let cause = error.to_string();
            if let Err(recovery) = self.recover_partial_resume() {
                let _ = self.kill_inferior();
                return Err(backend_error(LinuxError::ResumeRecovery {
                    cause,
                    recovery: recovery.to_string(),
                }));
            }
            return Err(error);
        }
        self.bump_revision();
        Ok(execution_id)
    }

    fn restore_unconsumed_signals(&mut self, suppressed: &[(Pid, PendingSignal)]) {
        let Some(inferior) = self.inferior.as_mut() else {
            return;
        };
        for &(pid, pending) in suppressed {
            if let Some(thread) = inferior.threads.get_mut(&pid)
                && matches!(thread.state, NativeThreadState::Stopped)
                && thread.pending_signal.is_none()
            {
                thread.pending_signal = Some(pending);
            }
        }
    }

    fn reply_execution(
        &self,
        result: Result<ExecutionId>,
        scope: ResumeScope,
        reply: Reply<ExecutionId>,
    ) {
        match result {
            Ok(execution_id) => {
                let process_id = self
                    .inferior
                    .as_ref()
                    .map(|inferior| process_id(inferior.tgid))
                    .expect("successful execution has an inferior");
                let _ = reply.send(Ok(execution_id));
                let _ = self.events.send(DebuggerEvent::InferiorContinued {
                    revision: self.revision,
                    process_id,
                    execution_id,
                    resumed: scope,
                });
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn begin_pause(&mut self, requested_process: ProcessId) -> Result<ExecutionId> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        validate_process(inferior, requested_process)?;
        let execution_id = inferior.active.as_ref().ok_or(Error::NotStopped)?.id;
        if inferior.barrier.is_some() {
            return Ok(execution_id);
        }

        let triggering_thread = inferior
            .threads
            .iter()
            .find_map(|(&pid, thread)| {
                matches!(thread.state, NativeThreadState::Running).then_some(pid)
            })
            .ok_or(Error::NotStopped)?;
        inferior.next_barrier = inferior.next_barrier.wrapping_add(1);
        let barrier_id = inferior.next_barrier;
        inferior.barrier = Some(StopBarrier {
            execution: Some(execution_id),
            triggering_thread,
            reason: StopReason::Pause,
        });

        self.request_stops(barrier_id)?;
        self.finish_barrier_if_ready()?;
        Ok(execution_id)
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    fn advance_execution(&mut self) -> Result<()> {
        if !self
            .inferior
            .as_ref()
            .ok_or(Error::NotRunning)?
            .repairs
            .is_empty()
        {
            return self.start_next_repair();
        }

        let (kind, resume_threads) = {
            let active = self
                .inferior
                .as_ref()
                .and_then(|inferior| inferior.active.as_ref())
                .ok_or(Error::NotRunning)?;
            (active.kind.clone(), active.resume_threads.clone())
        };

        match kind {
            ActiveKind::Step { thread, kind, .. } => self.start_user_step(thread, kind),
            ActiveKind::Launch | ActiveKind::Continue => {
                if let Some(pid) = resume_threads.iter().copied().find(|pid| {
                    self.inferior
                        .as_ref()
                        .and_then(|inferior| inferior.threads.get(pid))
                        .is_some_and(|thread| thread.awaiting_breakpoint.is_some())
                }) {
                    self.resume_awaiting_thread(pid)
                } else {
                    for pid in resume_threads {
                        self.continue_thread(pid)?;
                    }
                    Ok(())
                }
            }
        }
    }

    fn start_next_repair(&mut self) -> Result<()> {
        let next = {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            let group = inferior.repairs.front_mut().expect("repair group exists");
            if group.current.is_some() {
                return Ok(());
            }
            group.remaining.pop_front().map(|pid| {
                group.current = Some(pid);
                (pid, group.address, !group.site_removed)
            })
        };

        let Some((pid, address, remove_site)) = next else {
            self.finish_repair_group()?;
            return self.advance_execution();
        };
        let needs_signal_delivery = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.threads.get(&pid))
            .is_some_and(|thread| thread.pending_signal.is_some());

        if needs_signal_delivery {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            let thread = inferior
                .threads
                .get_mut(&pid)
                .expect("repair thread exists");
            let signal = thread.pending_signal.map(|pending| pending.signal);
            self.ptrace.continue_execution(pid, signal)?;
            thread.pending_signal = None;
            thread.awaiting_breakpoint = Some(address);
            thread.stopped_at_breakpoint = None;
            thread.expected = ExpectedStop::AwaitBreakpoint { address };
            thread.state = NativeThreadState::Running;
            return Ok(());
        }

        if remove_site {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            self.ptrace
                .remove_breakpoint(inferior.tgid, &mut inferior.breakpoints, address)?;
            inferior
                .repairs
                .front_mut()
                .expect("repair group exists")
                .site_removed = true;
        }

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let thread = inferior
            .threads
            .get_mut(&pid)
            .expect("repair thread exists");
        self.ptrace.step(pid, None)?;
        thread.expected = ExpectedStop::BreakpointRepair { address };
        thread.state = NativeThreadState::Running;
        Ok(())
    }

    fn finish_repair_group(&mut self) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let group = inferior.repairs.pop_front().expect("repair group exists");
        if group.site_removed {
            self.ptrace.reinstall_breakpoint(
                inferior.tgid,
                &mut inferior.breakpoints,
                group.address,
            )?;
        }
        Ok(())
    }

    fn start_user_step(&mut self, pid: Pid, kind: StepKind) -> Result<()> {
        let uses_plan_breakpoints = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .is_some_and(|active| {
                matches!(
                    &active.kind,
                    ActiveKind::Step { start, .. } if !start.plan_addresses.is_empty()
                )
            });
        if uses_plan_breakpoints {
            return self.continue_thread(pid);
        }
        if kind == StepKind::IntoSource
            && self.stopped_outside_described_code(pid)?
            && self.escape_undescribed_code(pid)?
        {
            return Ok(());
        }

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let thread = inferior.threads.get_mut(&pid).ok_or(Error::NotRunning)?;
        let signal = thread.pending_signal.map(|pending| pending.signal);
        self.ptrace.step(pid, signal)?;
        thread.pending_signal = None;
        thread.expected = ExpectedStop::UserStep { kind };
        thread.state = NativeThreadState::Running;
        Ok(())
    }

    /// Reports whether the thread is stopped at an instruction that no DWARF
    /// code instance describes (PLT stubs, library code, assembly thunks).
    fn stopped_outside_described_code(&self, pid: Pid) -> Result<bool> {
        let registers = self.ptrace.registers(pid)?;
        Ok(self
            .image_location(VirtualAddress::new(registers.rip))
            .is_none_or(|location| {
                location.physical_instance.is_none() && location.source.is_none()
            }))
    }

    /// Runs to the caller instead of instruction-stepping through code without
    /// debug information. The return address comes from call-frame information
    /// when it covers the stopped address (PLT stubs), otherwise from the top
    /// of the stack, which holds the return address immediately after the call
    /// that entered the undescribed code. Either candidate is trusted only
    /// when it resolves to a described instruction. Returns false when no
    /// trustworthy return address exists and the caller should fall back to
    /// instruction stepping.
    fn escape_undescribed_code(&mut self, pid: Pid) -> Result<bool> {
        let registers = self.ptrace.registers(pid)?;
        let candidate = match self.caller_address(pid, &registers) {
            Ok(address) => address,
            Err(_) => match self.ptrace.read_word(pid, registers.rsp) {
                Ok(word) => VirtualAddress::new(word),
                Err(_) => return Ok(false),
            },
        };
        let described = self
            .image_location(candidate)
            .is_some_and(|location| location.physical_instance.is_some());
        if !described {
            return Ok(false);
        }
        let execution = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .map(|active| active.id)
            .ok_or(Error::NotRunning)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        self.ptrace.install_breakpoint(
            inferior.tgid,
            &mut inferior.breakpoints,
            candidate,
            BreakpointOwner::Plan(execution),
        )?;
        self.continue_thread(pid)?;
        Ok(true)
    }

    fn resume_awaiting_thread(&mut self, pid: Pid) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let thread = inferior
            .threads
            .get_mut(&pid)
            .expect("awaiting thread exists");
        let address = thread.awaiting_breakpoint.expect("breakpoint is awaited");
        let signal = thread.pending_signal.map(|pending| pending.signal);
        self.ptrace.continue_execution(pid, signal)?;
        thread.pending_signal = None;
        thread.expected = ExpectedStop::AwaitBreakpoint { address };
        thread.state = NativeThreadState::Running;
        Ok(())
    }

    fn continue_thread(&mut self, pid: Pid) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let thread = inferior.threads.get_mut(&pid).ok_or(Error::NotRunning)?;
        if !matches!(thread.state, NativeThreadState::Stopped) {
            return Ok(());
        }
        let signal = thread.pending_signal.map(|pending| pending.signal);
        self.ptrace.continue_execution(pid, signal)?;
        thread.pending_signal = None;
        thread.state = NativeThreadState::Running;
        thread.expected = ExpectedStop::None;
        thread.reason = None;
        Ok(())
    }

    fn classify_stop(&self, pid: Pid, signal: NixSignal) -> ClassifiedStop {
        let status = format!("Stopped({pid}, {signal})");
        let siginfo = self.ptrace.signal_metadata(pid);
        let expected = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.threads.get(&pid))
            .map_or(ExpectedStop::None, |thread| thread.expected.clone());
        let starting = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.threads.get(&pid))
            .is_some_and(|thread| matches!(thread.state, NativeThreadState::Starting));
        let debugger_requested = signal == NixSignal::SIGSTOP
            && self
                .inferior
                .as_ref()
                .and_then(|inferior| inferior.threads.get(&pid))
                .is_some_and(|thread| thread.debugger_stop_pending)
            && siginfo.as_ref().is_ok_and(|metadata| {
                metadata.code == libc::SI_TKILL
                    && metadata.sender
                        == Some(i32::try_from(std::process::id()).unwrap_or(i32::MAX))
            });
        let expected_trace = signal == NixSignal::SIGTRAP
            && siginfo.as_ref().is_ok_and(is_single_step_trap)
            && matches!(
                expected,
                ExpectedStop::BreakpointRepair { .. } | ExpectedStop::UserStep { .. }
            );
        let breakpoint = (signal == NixSignal::SIGTRAP && !expected_trace)
            .then(|| self.normalize_breakpoint_pc(pid))
            .flatten();

        classify_stop_evidence(
            signal,
            status,
            siginfo,
            &expected,
            starting,
            debugger_requested,
            breakpoint,
        )
    }

    fn normalize_breakpoint_pc(&self, pid: Pid) -> Option<VirtualAddress> {
        let mut registers = self.ptrace.registers(pid).ok()?;
        let address = VirtualAddress::new(registers.rip.checked_sub(1)?);
        let installed = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.breakpoints.get(&address))
            .is_some_and(|site| site.installed);
        if !installed {
            return None;
        }

        registers.rip = address.get();
        self.ptrace.set_registers(pid, registers).ok()?;
        Some(address)
    }

    fn handle_classified_stop(&mut self, pid: Pid, stop: ClassifiedStop) -> Result<()> {
        {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            inferior
                .threads
                .get_mut(&pid)
                .ok_or(Error::NotRunning)?
                .state = NativeThreadState::Stopped;
        }

        match stop {
            ClassifiedStop::ThreadStart => self.handle_thread_start(pid),
            ClassifiedStop::DebuggerRequested => {
                let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
                let thread = inferior
                    .threads
                    .get_mut(&pid)
                    .expect("stopped thread exists");
                thread.reason = None;
                thread.debugger_stop_pending = false;
                if inferior.barrier.is_some() {
                    self.finish_barrier_if_ready()
                } else {
                    self.restart_after_internal(pid)
                }
            }
            ClassifiedStop::Breakpoint(address) => self.handle_breakpoint_stop(pid, address),
            ClassifiedStop::Trace => self.handle_trace_stop(pid),
            ClassifiedStop::SignalDelivery(pending) => self.handle_signal_stop(pid, pending),
            ClassifiedStop::GroupStop(signal) => {
                self.begin_visible_stop(pid, StopReason::Exception(exception_info(signal)))
            }
            ClassifiedStop::Unclassifiable(raw) => self.begin_visible_stop(
                pid,
                StopReason::Unclassifiable {
                    description: format_raw_stop(&raw).into(),
                },
            ),
        }
    }

    fn handle_breakpoint_stop(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        let plan = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { thread, kind, .. } if *thread == pid => Some((active.id, *kind)),
                _ => None,
            })
            .filter(|(execution, _)| {
                self.inferior
                    .as_ref()
                    .and_then(|inferior| inferior.breakpoints.get(&address))
                    .is_some_and(|site| site.owners.contains(&BreakpointOwner::Plan(*execution)))
            });
        if let Some((execution, kind)) = plan {
            let has_user_owner = self
                .inferior
                .as_ref()
                .and_then(|inferior| inferior.breakpoints.get(&address))
                .is_some_and(|site| {
                    site.owners
                        .iter()
                        .any(|owner| matches!(owner, BreakpointOwner::User(_)))
                });
            if !has_user_owner {
                self.inferior
                    .as_mut()
                    .and_then(|inferior| inferior.threads.get_mut(&pid))
                    .ok_or(Error::NotRunning)?
                    .stopped_at_breakpoint = Some(address);
                if self.step_is_complete(pid, kind)? {
                    self.cleanup_plan_breakpoints(execution)?;
                    self.inferior
                        .as_mut()
                        .and_then(|inferior| inferior.threads.get_mut(&pid))
                        .ok_or(Error::NotRunning)?
                        .stopped_at_breakpoint = None;
                    return self.begin_visible_stop(pid, StopReason::Step { kind });
                }

                self.queue_repair(pid, address);
                return self.start_next_repair();
            }
        }

        let awaited = {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            let thread = inferior
                .threads
                .get_mut(&pid)
                .expect("stopped thread exists");
            thread.stopped_at_breakpoint = Some(address);
            let awaited = thread.awaiting_breakpoint == Some(address)
                && matches!(thread.expected, ExpectedStop::AwaitBreakpoint { address: expected } if expected == address);
            if awaited {
                thread.awaiting_breakpoint = None;
                thread.expected = ExpectedStop::None;
            }
            awaited
        };

        if awaited {
            self.queue_repair(pid, address);
            self.start_next_repair()
        } else {
            self.begin_visible_stop(pid, StopReason::Breakpoint { address })
        }
    }

    fn handle_trace_stop(&mut self, pid: Pid) -> Result<()> {
        let expected = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.threads.get_mut(&pid))
            .map(|thread| std::mem::replace(&mut thread.expected, ExpectedStop::None))
            .ok_or(Error::NotRunning)?;

        if self.pause_barrier_active() {
            return self.settle_trace_during_pause(pid, expected);
        }

        match expected {
            ExpectedStop::BreakpointRepair { address } => self.complete_repair(pid, address),
            ExpectedStop::UserStep { kind } => self.complete_user_step(pid, kind),
            other => self.begin_visible_stop(
                pid,
                StopReason::Unclassifiable {
                    description: format!("unexpected trace stop in {other:?}").into(),
                },
            ),
        }
    }

    fn pause_barrier_active(&self) -> bool {
        self.inferior
            .as_ref()
            .and_then(|inferior| inferior.barrier.as_ref())
            .is_some_and(|barrier| barrier.reason == StopReason::Pause)
    }

    fn settle_trace_during_pause(&mut self, pid: Pid, expected: ExpectedStop) -> Result<()> {
        match expected {
            ExpectedStop::BreakpointRepair { address } => {
                let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
                let thread = inferior
                    .threads
                    .get_mut(&pid)
                    .expect("repair thread exists");
                thread.stopped_at_breakpoint = None;

                let group = inferior.repairs.front_mut().ok_or_else(|| {
                    backend_error(LinuxError::UnexpectedWait(
                        "breakpoint repair trace without a repair group".to_owned(),
                    ))
                })?;
                if group.address != address || group.current != Some(pid) {
                    return Err(backend_error(LinuxError::UnexpectedWait(format!(
                        "breakpoint repair trace for {pid} at {address:?} did not match the active repair"
                    ))));
                }
                group.current = None;
            }
            ExpectedStop::UserStep { .. } => {}
            other => {
                return self.begin_visible_stop(
                    pid,
                    StopReason::Unclassifiable {
                        description: format!("unexpected trace stop during pause in {other:?}")
                            .into(),
                    },
                );
            }
        }

        self.finish_barrier_if_ready()
    }

    fn remove_breakpoint_owner(
        &mut self,
        address: VirtualAddress,
        owner: BreakpointOwner,
    ) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        remove_breakpoint_owner_from(&self.ptrace, inferior, address, owner)
    }

    fn cleanup_plan_breakpoints(&mut self, execution: ExecutionId) -> Result<()> {
        let addresses = self
            .inferior
            .as_ref()
            .ok_or(Error::NotRunning)?
            .breakpoints
            .iter()
            .filter_map(|(&address, site)| {
                site.owners
                    .contains(&BreakpointOwner::Plan(execution))
                    .then_some(address)
            })
            .collect::<Vec<_>>();

        for address in addresses {
            self.remove_breakpoint_owner(address, BreakpointOwner::Plan(execution))?;
        }
        Ok(())
    }

    fn handle_signal_stop(&mut self, pid: Pid, pending: PendingSignal) -> Result<()> {
        let repair_address = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.threads.get(&pid))
            .and_then(|thread| match thread.expected {
                ExpectedStop::BreakpointRepair { address } => Some(address),
                _ => None,
            });
        if let Some(address) = repair_address {
            self.restore_active_breakpoints()?;
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            let thread = inferior
                .threads
                .get_mut(&pid)
                .expect("repair thread exists");
            thread.stopped_at_breakpoint = None;
            thread.awaiting_breakpoint = Some(address);
        }
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let thread = inferior
            .threads
            .get_mut(&pid)
            .expect("stopped thread exists");
        thread.expected = ExpectedStop::None;
        thread.pending_signal = Some(pending);
        self.begin_visible_stop(pid, StopReason::Exception(pending_exception_info(pending)))
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    fn handle_thread_start(&mut self, pid: Pid) -> Result<()> {
        self.ptrace.set_options(pid)?;
        let (process_id, barrier_active, should_resume) = {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            let thread = inferior.threads.get_mut(&pid).expect("new thread exists");
            thread.state = NativeThreadState::Stopped;
            thread.expected = ExpectedStop::None;
            let should_resume = inferior.active.as_ref().is_some_and(|active| {
                matches!(active.kind, ActiveKind::Launch | ActiveKind::Continue)
                    && active.resume_threads.contains(&pid)
            });
            (
                process_id(inferior.tgid),
                inferior.barrier.is_some(),
                should_resume,
            )
        };
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::ThreadStarted {
            revision: self.revision,
            process_id,
            thread_id: debug_thread_id(pid),
        });
        if barrier_active {
            self.finish_barrier_if_ready()
        } else if should_resume {
            self.continue_thread(pid)
        } else {
            Ok(())
        }
    }

    fn complete_repair(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            let thread = inferior
                .threads
                .get_mut(&pid)
                .expect("repair thread exists");
            thread.stopped_at_breakpoint = None;
            let group = inferior.repairs.front_mut().expect("repair group exists");
            assert_eq!(group.address, address);
            assert_eq!(group.current, Some(pid));
            group.current = None;
        }

        let step_kind = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match active.kind {
                ActiveKind::Step { thread, kind, .. } if thread == pid => Some(kind),
                _ => None,
            });
        let group_done = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.repairs.front())
            .is_some_and(|group| group.remaining.is_empty());
        if group_done {
            self.finish_repair_group()?;
            if let Some(kind) = step_kind {
                return self.complete_user_step(pid, kind);
            }
            return self.advance_execution();
        }
        self.start_next_repair()
    }

    fn complete_user_step(&mut self, pid: Pid, kind: StepKind) -> Result<()> {
        if self.step_is_complete(pid, kind)? {
            self.begin_visible_stop(pid, StopReason::Step { kind })
        } else {
            self.start_user_step(pid, kind)
        }
    }

    fn step_is_complete(&self, pid: Pid, kind: StepKind) -> Result<bool> {
        if kind == StepKind::Instruction {
            return Ok(true);
        }
        let registers = self.ptrace.registers(pid)?;
        let start = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { start, .. } => Some(start),
                _ => None,
            })
            .expect("source step has a starting state");

        match kind {
            StepKind::Instruction => Ok(true),
            StepKind::IntoSource => {
                let location = self.image_location(VirtualAddress::new(registers.rip));
                // Code that no DWARF instance describes (PLT stubs, library
                // code) is never a step destination; the step continues until
                // execution returns to described code.
                if location.as_ref().is_none_or(|location| {
                    location.physical_instance.is_none() && location.source.is_none()
                }) {
                    return Ok(false);
                }
                let presentation = self.presentation_for_thread(
                    pid,
                    &StopReason::Step {
                        kind: StepKind::IntoSource,
                    },
                )?;
                let current_instance = location
                    .as_ref()
                    .map(|location| selected_code_instance(location, &presentation))
                    .transpose()?
                    .flatten();
                let source = location.as_ref().and_then(|location| {
                    current_instance.and_then(|instance| {
                        source_for_code_instance(&self.module_image, location, instance)
                    })
                });
                let activation = self.top_activation(pid, &registers)?;
                let statement = location.as_ref().is_some_and(|location| {
                    self.module_image
                        .line_entry_containing(location.address)
                        .is_some_and(|entry| entry.statement)
                });

                Ok(statement
                    && (activation != start.activation.unwrap_or(activation)
                        || current_instance != start.code_instance
                        || source_line_changed(start.source.as_ref(), source.as_ref())))
            }
            StepKind::OverSource | StepKind::Out => {
                let Some(activation) = start.activation else {
                    return Err(Error::LocationUnavailable);
                };
                let Some(code_instance) = start.code_instance else {
                    return Err(Error::LocationUnavailable);
                };
                let Some(location) = self.location_for_activation(pid, &registers, activation)?
                else {
                    return Ok(true);
                };
                let source = source_for_code_instance(&self.module_image, &location, code_instance);

                Ok(source.is_none()
                    || (kind == StepKind::OverSource
                        && source_line_changed(start.source.as_ref(), source.as_ref())))
            }
        }
    }

    fn step_start(&self, pid: Pid, kind: StepKind) -> Result<StepStart> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let thread = inferior.threads.get(&pid).ok_or(Error::NotRunning)?;
        if !matches!(thread.state, NativeThreadState::Stopped) {
            return Err(Error::NotStopped);
        }
        let registers = self.ptrace.registers(pid)?;
        let location = self.image_location(VirtualAddress::new(registers.rip));
        let presentation = self.presentation_for_stopped_thread(pid)?;
        let code_instance = location
            .as_ref()
            .map(|location| selected_code_instance(location, &presentation))
            .transpose()?
            .flatten();
        let source = location.as_ref().and_then(|location| {
            code_instance.and_then(|instance| {
                source_for_code_instance(&self.module_image, location, instance)
            })
        });
        let activation = (kind != StepKind::Instruction)
            .then(|| self.top_activation(pid, &registers))
            .transpose()?;
        let mut plan_addresses = BTreeSet::new();

        let selected_is_inline = code_instance
            .and_then(|instance| self.module_image.code_instance(instance))
            .is_some_and(|instance| matches!(instance.kind, CodeInstanceKind::Inline { .. }));
        if kind == StepKind::Out && !selected_is_inline {
            plan_addresses.insert(self.caller_address(pid, &registers)?);
        } else if kind == StepKind::OverSource
            && let (Some(source), Some(instance_id)) = (&source, code_instance)
            && let Some(instance) = self.module_image.code_instance(instance_id)
        {
            let return_address = self.caller_address(pid, &registers)?;
            for line in self.module_image.line_entries() {
                if !line.statement || !instance.contains(line.range.start) {
                    continue;
                }
                let location = self.module_image.locate(line.range.start);
                if source_for_code_instance(&self.module_image, &location, instance_id)
                    .is_some_and(|candidate| source_line_changed(Some(source), Some(&candidate)))
                {
                    plan_addresses
                        .insert(inferior.loaded_module.virtual_address(line.range.start)?);
                }
            }
            plan_addresses.insert(return_address);
        }

        Ok(StepStart {
            source,
            code_instance,
            activation,
            plan_addresses,
        })
    }

    fn top_activation(&self, pid: Pid, native: &libc::user_regs_struct) -> Result<VirtualAddress> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let current = FrameContext {
            instruction: VirtualAddress::new(native.rip),
            cfa: None,
            signal_frame: false,
        };
        let mut provider = DwarfCallerProvider {
            unwind_info: self.unwind_info.as_ref(),
            loaded_module: inferior.loaded_module,
            module_image: &self.module_image,
            registers: x86_64_registers(native),
            memory: PtraceMemory {
                ptrace: &self.ptrace,
                pid,
            },
            first: true,
        };

        match provider.caller(&current) {
            CallerResult::Caller(caller) => caller.cfa.ok_or(Error::LocationUnavailable),
            CallerResult::Finished(reason) => {
                Err(backend_error(LinuxError::CallerUnavailable(reason)))
            }
        }
    }

    fn location_for_activation(
        &self,
        pid: Pid,
        native: &libc::user_regs_struct,
        activation: VirtualAddress,
    ) -> Result<Option<ImageLocation>> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let mut context = FrameContext {
            instruction: VirtualAddress::new(native.rip),
            cfa: None,
            signal_frame: false,
        };
        let mut provider = DwarfCallerProvider {
            unwind_info: self.unwind_info.as_ref(),
            loaded_module: inferior.loaded_module,
            module_image: &self.module_image,
            registers: x86_64_registers(native),
            memory: PtraceMemory {
                ptrace: &self.ptrace,
                pid,
            },
            first: true,
        };

        for level in 0..DEFAULT_MAX_FRAMES {
            let caller = match provider.caller(&context) {
                CallerResult::Caller(caller) => caller,
                CallerResult::Finished(reason) => {
                    return Err(backend_error(LinuxError::CallerUnavailable(reason)));
                }
            };
            if caller.cfa == Some(activation) {
                let level = u32::try_from(level).expect("frame limit fits u32");
                let location = frame_lookup_address(level, &context)
                    .and_then(|address| inferior.loaded_module.image_address(address).ok())
                    .filter(|address| self.module_image.contains_address(*address))
                    .map(|address| self.module_image.locate(address));

                return Ok(location);
            }
            // This backend only supports x86-64's downward-growing ordinary stack. Once
            // unwinding passes the starting CFA, that activation has returned.
            if caller.cfa.is_some_and(|cfa| cfa > activation) {
                return Ok(None);
            }
            context = caller;
        }

        Ok(None)
    }

    fn caller_address(&self, pid: Pid, native: &libc::user_regs_struct) -> Result<VirtualAddress> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let current = FrameContext {
            instruction: VirtualAddress::new(native.rip),
            cfa: None,
            signal_frame: false,
        };
        let mut provider = DwarfCallerProvider {
            unwind_info: self.unwind_info.as_ref(),
            loaded_module: inferior.loaded_module,
            module_image: &self.module_image,
            registers: x86_64_registers(native),
            memory: PtraceMemory {
                ptrace: &self.ptrace,
                pid,
            },
            first: true,
        };

        match provider.caller(&current) {
            CallerResult::Caller(caller) => Ok(caller.instruction),
            CallerResult::Finished(reason) => {
                Err(backend_error(LinuxError::CallerUnavailable(reason)))
            }
        }
    }

    fn image_location(&self, address: VirtualAddress) -> Option<ImageLocation> {
        let inferior = self.inferior.as_ref()?;
        let image = inferior.loaded_module.image_address(address).ok()?;
        Some(self.module_image.locate(image))
    }

    fn queue_repair(&mut self, pid: Pid, address: VirtualAddress) {
        let inferior = self.inferior.as_mut().expect("inferior exists");
        if let Some(group) = inferior
            .repairs
            .iter_mut()
            .find(|group| group.address == address)
        {
            group.remaining.push_front(pid);
        } else {
            inferior.repairs.push_front(RepairGroup {
                address,
                remaining: VecDeque::from([pid]),
                current: None,
                site_removed: false,
            });
        }
    }

    fn begin_visible_stop(&mut self, pid: Pid, reason: StopReason) -> Result<()> {
        let barrier_id = {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            let thread = inferior.threads.get_mut(&pid).ok_or(Error::NotRunning)?;
            thread.state = NativeThreadState::Stopped;
            thread.reason = Some(reason.clone());
            thread.expected = ExpectedStop::None;

            if let Some(barrier) = inferior.barrier.as_mut() {
                if barrier.reason == StopReason::Pause && reason != StopReason::Pause {
                    barrier.triggering_thread = pid;
                    barrier.reason = reason;
                }
                None
            } else {
                inferior.next_barrier = inferior.next_barrier.wrapping_add(1);
                inferior.barrier = Some(StopBarrier {
                    execution: inferior.active.as_ref().map(|active| active.id),
                    triggering_thread: pid,
                    reason,
                });
                Some(inferior.next_barrier)
            }
        };
        if let Some(barrier_id) = barrier_id {
            self.request_stops(barrier_id)?;
        }
        self.finish_barrier_if_ready()
    }

    fn recover_partial_resume(&mut self) -> Result<()> {
        self.restore_active_breakpoints()?;
        let barrier_id = {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            let execution = inferior.active.as_ref().map(|active| active.id);
            let triggering_thread = inferior
                .threads
                .iter()
                .find_map(|(&pid, thread)| {
                    matches!(thread.state, NativeThreadState::Stopped).then_some(pid)
                })
                .or_else(|| inferior.threads.keys().next().copied())
                .ok_or(Error::NotRunning)?;
            let reason = StopReason::Unclassifiable {
                description: "execution resume failed; the debugger recovered to all-stop".into(),
            };
            inferior
                .threads
                .get_mut(&triggering_thread)
                .expect("recovery thread exists")
                .reason = Some(reason.clone());
            inferior.next_barrier = inferior.next_barrier.wrapping_add(1);
            let barrier_id = inferior.next_barrier;
            inferior.barrier = Some(StopBarrier {
                execution,
                triggering_thread,
                reason,
            });
            barrier_id
        };
        self.request_stops(barrier_id)?;
        self.finish_barrier_if_ready()
    }

    fn restore_active_breakpoints(&mut self) -> Result<()> {
        let Some(inferior) = self.inferior.as_mut() else {
            return Ok(());
        };
        let removed: Vec<_> = inferior
            .repairs
            .iter()
            .filter(|group| group.site_removed)
            .map(|group| group.address)
            .collect();
        for address in removed {
            self.ptrace
                .reinstall_breakpoint(inferior.tgid, &mut inferior.breakpoints, address)?;
        }
        inferior.repairs.clear();
        Ok(())
    }

    fn request_stops(&mut self, barrier_id: u64) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let tgid = inferior.tgid;
        let running: Vec<_> = inferior
            .threads
            .iter()
            .filter_map(|(&pid, thread)| {
                matches!(thread.state, NativeThreadState::Running).then_some(pid)
            })
            .collect();
        for pid in running {
            self.ptrace.request_stop(tgid, pid)?;
            let thread = inferior.threads.get_mut(&pid).expect("thread exists");
            thread.debugger_stop_pending = true;
            thread.state = NativeThreadState::StopRequested {
                barrier: barrier_id,
            };
        }
        Ok(())
    }

    fn finish_barrier_if_ready(&mut self) -> Result<()> {
        let ready = self.inferior.as_ref().is_some_and(|inferior| {
            inferior.barrier.is_some()
                && inferior
                    .threads
                    .values()
                    .all(|thread| matches!(thread.state, NativeThreadState::Stopped))
        });
        if !ready {
            return Ok(());
        }

        self.restore_active_breakpoints()?;
        if let Some(execution) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .map(|active| active.id)
        {
            self.cleanup_plan_breakpoints(execution)?;
        }
        let (triggering_thread, reason) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.barrier.as_ref())
            .map(|barrier| (barrier.triggering_thread, barrier.reason.clone()))
            .expect("ready barrier exists");
        let presentation = self.presentation_for_thread(triggering_thread, &reason)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        for thread in inferior.threads.values_mut() {
            thread.expected = ExpectedStop::None;
        }
        let barrier = inferior.barrier.take().expect("barrier exists");
        inferior.next_stop = inferior.next_stop.wrapping_add(1);
        let stop_id = StopId::new(inferior.next_stop);
        inferior.public_stop = Some(PublicStop {
            id: stop_id,
            triggering_thread: barrier.triggering_thread,
            reason: barrier.reason.clone(),
            presentations: BTreeMap::from([(triggering_thread, presentation)]),
        });
        inferior.selected_thread = Some(barrier.triggering_thread);
        inferior.active = None;
        let process_id = process_id(inferior.tgid);
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::InferiorStopped {
            revision: self.revision,
            process_id,
            execution_id: barrier.execution,
            stop_id,
            thread_id: debug_thread_id(barrier.triggering_thread),
            all_threads_stopped: true,
            reason: barrier.reason,
        });
        Ok(())
    }

    fn handle_ptrace_event(&mut self, pid: Pid, event: i32) -> Result<()> {
        match event {
            libc::PTRACE_EVENT_CLONE => {
                self.inferior
                    .as_mut()
                    .and_then(|inferior| inferior.threads.get_mut(&pid))
                    .ok_or(Error::NotRunning)?
                    .state = NativeThreadState::Stopped;
                self.handle_clone_event(pid)
            }
            libc::PTRACE_EVENT_EXEC => {
                self.inferior
                    .as_mut()
                    .and_then(|inferior| inferior.threads.get_mut(&pid))
                    .ok_or(Error::NotRunning)?
                    .state = NativeThreadState::Stopped;
                self.handle_exec_event(pid)
            }
            libc::PTRACE_EVENT_EXIT => {
                let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
                inferior
                    .threads
                    .get_mut(&pid)
                    .ok_or(Error::NotRunning)?
                    .state = NativeThreadState::Exiting;
                self.ptrace.continue_execution(pid, None)
            }
            other => self.begin_visible_stop(
                pid,
                StopReason::Unclassifiable {
                    description: format!("unsupported ptrace event {other}").into(),
                },
            ),
        }
    }

    fn handle_clone_event(&mut self, parent: Pid) -> Result<()> {
        let child = Pid::from_raw(
            i32::try_from(self.ptrace.event_message(parent)?)
                .map_err(|_| Error::AddressOverflow)?,
        );
        let child_tgid = self.ptrace.thread_group_id(child)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        if child_tgid != inferior.tgid {
            let _ = self.ptrace.kill(child, NixSignal::SIGKILL);
            return self.begin_visible_stop(
                parent,
                StopReason::Unclassifiable {
                    description: backend_error(LinuxError::UnsupportedClone(child_tgid.as_raw()))
                        .to_string()
                        .into(),
                },
            );
        }
        assert!(
            inferior
                .threads
                .insert(child, TraceThread::starting(ExpectedStop::None))
                .is_none(),
            "clone TID is unique"
        );
        if let Some(active) = inferior.active.as_mut()
            && matches!(active.kind, ActiveKind::Launch | ActiveKind::Continue)
            && matches!(active.scope, ResumeScope::Process(_))
        {
            active.resume_threads.insert(child);
        }
        let pending = inferior.unowned_stops.remove(&child);
        let barrier_active = inferior.barrier.is_some();
        if barrier_active {
            inferior
                .threads
                .get_mut(&parent)
                .expect("parent exists")
                .state = NativeThreadState::Stopped;
        } else {
            self.restart_after_internal(parent)?;
        }
        if let Some(status) = pending {
            self.process_wait(status)?;
        }
        Ok(())
    }

    fn handle_exec_event(&mut self, pid: Pid) -> Result<()> {
        let old_tid = Pid::from_raw(
            i32::try_from(self.ptrace.event_message(pid)?).map_err(|_| Error::AddressOverflow)?,
        );
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let mut survivor = inferior
            .threads
            .remove(&old_tid)
            .or_else(|| inferior.threads.remove(&pid))
            .unwrap_or_else(|| TraceThread::starting(ExpectedStop::None));
        inferior
            .retired_threads
            .extend(inferior.threads.keys().copied());
        inferior.threads.clear();
        survivor.state = NativeThreadState::Stopped;
        survivor.expected = ExpectedStop::None;
        survivor.pending_signal = None;
        survivor.stopped_at_breakpoint = None;
        survivor.awaiting_breakpoint = None;
        survivor.debugger_stop_pending = false;
        inferior.threads.insert(pid, survivor);
        inferior.breakpoints.clear();
        inferior.repairs.clear();
        inferior.exec_unsupported = true;
        self.begin_visible_stop(pid, StopReason::Exec)
    }

    fn restart_after_internal(&mut self, pid: Pid) -> Result<()> {
        let expected = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.threads.get(&pid))
            .map(|thread| thread.expected.clone())
            .ok_or(Error::NotRunning)?;
        match expected {
            ExpectedStop::UserStep { kind } => self.start_user_step(pid, kind),
            ExpectedStop::BreakpointRepair { .. } => {
                self.inferior
                    .as_mut()
                    .and_then(|inferior| inferior.threads.get_mut(&pid))
                    .ok_or(Error::NotRunning)?
                    .state = NativeThreadState::Running;
                self.ptrace.step(pid, None)
            }
            ExpectedStop::AwaitBreakpoint { .. } => self.resume_awaiting_thread(pid),
            ExpectedStop::InitialExec | ExpectedStop::None => self.continue_thread(pid),
        }
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    fn handle_terminal(&mut self, pid: Pid, status: ExitStatus) -> Result<()> {
        let (process_id, execution, thread_scoped, barrier_active, remaining) = {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            inferior.threads.remove(&pid).ok_or(Error::NotRunning)?;
            (
                process_id(inferior.tgid),
                inferior.active.as_ref().map(|active| active.id),
                inferior.active.as_ref().is_some_and(|active| {
                    matches!(active.scope, ResumeScope::Thread(thread) if thread == debug_thread_id(pid))
                }),
                inferior.barrier.is_some(),
                inferior.threads.len(),
            )
        };

        if remaining == 0 {
            let mut inferior = self.inferior.take().expect("inferior exists");
            if let Some(waiter) = inferior.waiter.take() {
                waiter.join().map_err(|_| Error::BackendThreadPanicked)?;
            }
            self.bump_revision();
            let _ = self.events.send(DebuggerEvent::InferiorExited {
                revision: self.revision,
                process_id,
                execution_id: execution,
                status,
            });
            if let Some(reply) = self.shutdown_reply.take() {
                let _ = reply.send(Ok(()));
            }
            return Ok(());
        }

        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::ThreadExited {
            revision: self.revision,
            process_id,
            thread_id: debug_thread_id(pid),
            status: status.clone(),
        });
        if barrier_active {
            return self.finish_barrier_if_ready();
        }
        if thread_scoped {
            let triggering_thread = self
                .inferior
                .as_ref()
                .and_then(|inferior| {
                    inferior.threads.iter().find_map(|(&pid, thread)| {
                        matches!(thread.state, NativeThreadState::Stopped).then_some(pid)
                    })
                })
                .ok_or(Error::NotRunning)?;
            return self.begin_visible_stop(
                triggering_thread,
                StopReason::ThreadExited {
                    thread_id: debug_thread_id(pid),
                    status,
                },
            );
        }
        Ok(())
    }

    fn read_word(
        &self,
        requested_process: ProcessId,
        stop_id: StopId,
        address: VirtualAddress,
    ) -> Result<u64> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_process(inferior, requested_process)?;
        validate_public_stop(inferior, Some(stop_id))?;
        let pid = inferior.selected_thread.ok_or(Error::NotStopped)?;
        let bytes = read_logical_memory(
            &self.ptrace,
            pid,
            &inferior.breakpoints,
            address,
            std::mem::size_of::<u64>(),
        )?;
        Ok(u64::from_le_bytes(
            bytes.try_into().expect("one native word was requested"),
        ))
    }

    fn write_word(
        &mut self,
        requested_process: ProcessId,
        stop_id: StopId,
        address: VirtualAddress,
        value: u64,
    ) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        validate_process(inferior, requested_process)?;
        validate_public_stop(inferior, Some(stop_id))?;
        let pid = inferior.selected_thread.ok_or(Error::NotStopped)?;
        let user_bytes = value.to_ne_bytes();
        let mut physical_bytes = user_bytes;

        for (&site_address, site) in &mut inferior.breakpoints {
            let Some(offset) = site_address.get().checked_sub(address.get()) else {
                continue;
            };
            if site.installed && offset < physical_bytes.len() as u64 {
                let offset = usize::try_from(offset).expect("word offset fits usize");
                site.original_byte = user_bytes[offset];
                physical_bytes[offset] = BREAKPOINT_OPCODE;
            }
        }

        self.ptrace
            .write_word(pid, address.get(), u64::from_ne_bytes(physical_bytes))
    }

    fn presentation_for_stopped_thread(&self, pid: Pid) -> Result<FramePresentation> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, inferior.public_stop.as_ref().map(|stop| stop.id))?;
        validate_stopped_thread(inferior, pid)?;
        let stop = inferior
            .public_stop
            .as_ref()
            .expect("public stop was validated");

        stop.presentations.get(&pid).cloned().map_or_else(
            || {
                let reason = inferior
                    .threads
                    .get(&pid)
                    .and_then(|thread| thread.reason.as_ref())
                    .unwrap_or(&stop.reason);
                self.presentation_for_thread(pid, reason)
            },
            Ok,
        )
    }

    fn presentation_for_thread(&self, pid: Pid, reason: &StopReason) -> Result<FramePresentation> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let registers = self.ptrace.registers(pid)?;
        let instruction = VirtualAddress::new(registers.rip);
        let image_address = inferior.loaded_module.image_address(instruction)?;
        let location = self.module_image.locate(image_address);

        let InlineFrameLookup::Unique(chain) = &location.inline_frames else {
            return Ok(match &location.inline_frames {
                InlineFrameLookup::Ambiguous(chains) => FramePresentation {
                    instruction,
                    frame: PresentedFrame::Ambiguous(
                        chains
                            .iter()
                            .flat_map(|chain| chain.instances.iter().copied())
                            .collect::<BTreeSet<_>>()
                            .into_iter()
                            .collect::<Vec<_>>()
                            .into(),
                    ),
                    hidden_inline_frames: 0,
                },
                InlineFrameLookup::None => FramePresentation {
                    instruction,
                    frame: PresentedFrame::Physical,
                    hidden_inline_frames: 0,
                },
                InlineFrameLookup::Unique(_) => unreachable!("matched above"),
            });
        };

        let breakpoint_targets = match reason {
            StopReason::Breakpoint { address } => {
                self.breakpoint_code_instances(inferior, *address)?
            }
            _ => BTreeSet::new(),
        };
        if !breakpoint_targets.is_empty() {
            let active = location
                .physical_instance
                .into_iter()
                .chain(chain.instances.iter().copied())
                .filter(|instance| breakpoint_targets.contains(instance))
                .collect::<Vec<_>>();

            if active.len() > 1 {
                return Ok(FramePresentation {
                    instruction,
                    frame: PresentedFrame::Ambiguous(active.into()),
                    hidden_inline_frames: 0,
                });
            }
            if let Some(target) = active.first().copied() {
                let visible = chain
                    .instances
                    .iter()
                    .position(|instance| *instance == target)
                    .map_or(0, |index| index + 1);

                return make_presentation(instruction, chain.instances.as_ref(), visible);
            }
        }

        let visible = chain
            .instances
            .iter()
            .position(|instance| {
                self.module_image
                    .code_instance(*instance)
                    .is_some_and(|instance| {
                        instance
                            .ranges
                            .iter()
                            .any(|range| range.start == image_address)
                    })
            })
            .unwrap_or(chain.instances.len());

        make_presentation(instruction, chain.instances.as_ref(), visible)
    }

    fn breakpoint_code_instances(
        &self,
        inferior: &Inferior,
        address: VirtualAddress,
    ) -> Result<BTreeSet<CodeInstanceId>> {
        let Some(site) = inferior.breakpoints.get(&address) else {
            return Ok(BTreeSet::new());
        };
        let mut instances = BTreeSet::new();

        for id in site.owners.iter().filter_map(|owner| match owner {
            BreakpointOwner::User(id) => Some(*id),
            BreakpointOwner::Plan(_) => None,
        }) {
            let breakpoint = self
                .breakpoints
                .iter()
                .find(|breakpoint| breakpoint.id == id)
                .expect("physical user owner references a logical breakpoint");
            for resolved in breakpoint.locations.iter() {
                if runtime_breakpoint_address(inferior, resolved.location)? == address {
                    instances.extend(resolved.code_instances.iter().copied());
                }
            }
        }

        Ok(instances)
    }

    fn stopped_location(&self, stop_id: StopId, pid: Pid) -> Result<ExecutionLocation> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_stopped_thread(inferior, pid)?;
        let registers = self.ptrace.registers(pid)?;
        let address = VirtualAddress::new(registers.rip);
        let image_address = inferior.loaded_module.image_address(address)?;
        let mut image = self.module_image.locate(image_address);
        let presentation = self.presentation_for_stopped_thread(pid)?;
        apply_presentation(&self.module_image, &mut image, &presentation)?;

        Ok(ExecutionLocation {
            module: inferior.loaded_module.id,
            address,
            image,
        })
    }

    fn loaded_module(&self) -> Result<LoadedModule> {
        self.inferior
            .as_ref()
            .map(|inferior| inferior.loaded_module)
            .ok_or(Error::NotRunning)
    }

    fn snapshot(&self) -> StateSnapshot {
        let Some(inferior) = self.inferior.as_ref() else {
            return StateSnapshot {
                revision: self.revision,
                inferior: InferiorState::NotRunning,
                stop_id: None,
                selected_thread: None,
                threads: Arc::from([]),
                presentation: None,
                breakpoints: self.breakpoints.clone().into(),
            };
        };
        let process_id = process_id(inferior.tgid);
        let state = inferior.public_stop.as_ref().map_or_else(
            || InferiorState::Running {
                process_id,
                execution_id: inferior.active.as_ref().map(|active| active.id),
            },
            |stop| InferiorState::Stopped {
                process_id,
                stop_id: stop.id,
                thread_id: debug_thread_id(stop.triggering_thread),
                all_threads_stopped: true,
                reason: stop.reason.clone(),
            },
        );
        let threads = inferior
            .threads
            .iter()
            .map(|(&pid, thread)| ThreadSnapshot {
                id: debug_thread_id(pid),
                state: if matches!(thread.state, NativeThreadState::Stopped) {
                    ObservableThreadState::Stopped {
                        reason: thread.reason.clone(),
                    }
                } else {
                    ObservableThreadState::Running
                },
            })
            .collect::<Vec<_>>()
            .into();

        StateSnapshot {
            revision: self.revision,
            inferior: state,
            stop_id: inferior.public_stop.as_ref().map(|stop| stop.id),
            selected_thread: inferior.selected_thread.map(debug_thread_id),
            threads,
            presentation: inferior.selected_thread.and_then(|pid| {
                inferior
                    .public_stop
                    .as_ref()
                    .and_then(|stop| stop.presentations.get(&pid))
                    .cloned()
            }),
            breakpoints: self.breakpoints.clone().into(),
        }
    }

    fn backtrace(&self, stop_id: StopId, pid: Pid) -> Result<Backtrace> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_stopped_thread(inferior, pid)?;
        let presentation = self.presentation_for_stopped_thread(pid)?;

        let native = self.ptrace.registers(pid)?;
        let registers = x86_64_registers(&native);
        let initial = FrameContext {
            instruction: VirtualAddress::new(native.rip),
            cfa: None,
            signal_frame: false,
        };
        let mut provider = DwarfCallerProvider {
            unwind_info: self.unwind_info.as_ref(),
            loaded_module: inferior.loaded_module,
            module_image: &self.module_image,
            registers,
            memory: PtraceMemory {
                ptrace: &self.ptrace,
                pid,
            },
            first: true,
        };
        let module_image = Arc::clone(&self.module_image);
        let loaded_module = inferior.loaded_module;

        let physical = collect_backtrace(
            debug_thread_id(pid),
            initial,
            &mut provider,
            |level, context| {
                let lookup = frame_lookup_address(level, context);
                let location = lookup
                    .and_then(|lookup| loaded_module.image_address(lookup).ok())
                    .filter(|address| module_image.contains_address(*address))
                    .map(|address| module_image.locate(address));
                StackFrame::new(
                    level,
                    if context.signal_frame {
                        FrameKind::Signal
                    } else {
                        FrameKind::Physical
                    },
                    location.as_ref().map(|_| loaded_module.id),
                    context.instruction,
                    location,
                )
            },
            DEFAULT_MAX_FRAMES,
        );

        expand_inline_backtrace(
            physical,
            &self.module_image,
            inferior.loaded_module,
            &presentation,
        )
    }

    fn registers(&self, stop_id: StopId, pid: Pid) -> Result<RegisterSnapshot> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_stopped_thread(inferior, pid)?;
        let native = self.ptrace.registers(pid)?;
        Ok(x86_64_register_snapshot(
            self.revision,
            pid,
            self.module_image.target(),
            &native,
        ))
    }

    fn variables(
        &self,
        stop_id: StopId,
        pid: Pid,
        query: &VariableQuery,
    ) -> Result<VariableSnapshot> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_stopped_thread(inferior, pid)?;
        // Source-level visibility follows the selected logical frame: an
        // inline presentation scopes lookup to that instance's variables, a
        // physical presentation to the containing function's own variables.
        // An ambiguous presentation has no single active scope chain.
        let presentation = self.presentation_for_stopped_thread(pid)?;
        let frame = presentation.frame;
        let selected_instance = match &frame {
            PresentedFrame::Physical => None,
            PresentedFrame::Inline(instance) => Some(*instance),
            PresentedFrame::Ambiguous(_) => return Err(Error::VariableContextUnsupported),
        };
        let native = self.ptrace.registers(pid)?;
        let registers = x86_64_registers(&native);
        let instruction = VirtualAddress::new(native.rip);
        let image_address = inferior.loaded_module.image_address(instruction)?;
        if !self.module_image.contains_address(image_address) {
            return Err(Error::LocationUnavailable);
        }
        let cfa = self
            .unwind_info
            .cfa(image_address, &registers)
            .map_err(|termination| match termination {
                UnwindTermination::UnsupportedUnwindInfo { feature }
                    if feature.as_ref() == "CFA expression" =>
                {
                    VariableUnavailableReason::CfaExpression
                }
                other => VariableUnavailableReason::Other(format!("{other:?}").into()),
            });
        let mut runtime = LinuxVariableRuntime {
            ptrace: &self.ptrace,
            pid,
            loaded_module: inferior.loaded_module,
            breakpoints: &inferior.breakpoints,
            native: &native,
            floating: None,
            cfa,
        };
        let variables =
            self.variable_info
                .inspect(image_address, selected_instance, query, &mut runtime)?;
        Ok(VariableSnapshot {
            revision: self.revision,
            stop_id,
            thread: debug_thread_id(pid),
            frame,
            target: self.module_image.target(),
            variables: variables.into(),
        })
    }

    fn select_thread(&mut self, stop_id: StopId, pid: Pid) -> Result<()> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_stopped_thread(inferior, pid)?;
        let presentation = self.presentation_for_stopped_thread(pid)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior
            .public_stop
            .as_mut()
            .expect("public stop was validated")
            .presentations
            .insert(pid, presentation);
        inferior.selected_thread = Some(pid);
        self.bump_revision();
        Ok(())
    }

    fn begin_shutdown(&mut self, reply: Option<Reply<()>>) {
        self.shutdown_reply = reply;
        self.launch_reply
            .take()
            .map(|reply| reply.send(Err(Error::RequestCancelled)));

        if self.inferior.is_some() {
            if let Err(error) = self.kill_inferior()
                && let Some(reply) = self.shutdown_reply.take()
            {
                let _ = reply.send(Err(error));
            }
        } else if let Some(reply) = self.shutdown_reply.take() {
            let _ = reply.send(Ok(()));
        }
    }

    fn handle_shutdown_wait(&mut self, status: WaitStatus) -> bool {
        let result = match status {
            WaitStatus::Exited(pid, code) => {
                self.handle_terminal(pid, ExitStatus::Code(i64::from(code)))
            }
            WaitStatus::Signaled(pid, signal, _) => {
                self.handle_terminal(pid, ExitStatus::Terminated(exception_info(signal)))
            }
            WaitStatus::PtraceEvent(pid, _, event) if event == libc::PTRACE_EVENT_EXIT => {
                self.ptrace.continue_during_shutdown(pid)
            }
            WaitStatus::Stopped(pid, _) | WaitStatus::PtraceEvent(pid, _, _) => self
                .kill_inferior()
                .and_then(|()| self.ptrace.continue_during_shutdown(pid)),
            other => Err(backend_error(LinuxError::UnexpectedWait(format!(
                "{other:?}"
            )))),
        };
        if result.is_err() {
            if let Some(reply) = self.shutdown_reply.take() {
                let _ = reply.send(result);
            }
            return false;
        }
        self.inferior.is_some()
    }

    fn kill_inferior(&self) -> Result<()> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(());
        };
        self.ptrace.kill(inferior.tgid, NixSignal::SIGKILL)
    }

    fn fail_inferior(&mut self, error: Error) {
        if let Some(reply) = self.launch_reply.take() {
            let _ = reply.send(Err(error));
        }
        let _ = self.kill_inferior();
    }

    fn bump_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        let _ = self.events.send(DebuggerEvent::StateChanged {
            revision: self.revision,
        });
    }
}

fn frame_lookup_address(level: u32, context: &FrameContext) -> Option<VirtualAddress> {
    if level == 0 || context.signal_frame {
        Some(context.instruction)
    } else {
        context
            .instruction
            .get()
            .checked_sub(1)
            .map(VirtualAddress::new)
    }
}

fn make_presentation(
    instruction: VirtualAddress,
    inline_chain: &[CodeInstanceId],
    visible: usize,
) -> Result<FramePresentation> {
    let hidden = inline_chain
        .len()
        .checked_sub(visible)
        .ok_or(Error::LocationUnavailable)?;
    let hidden_inline_frames = u32::try_from(hidden).map_err(|_| Error::LocationUnavailable)?;
    let frame = visible
        .checked_sub(1)
        .map_or(PresentedFrame::Physical, |index| {
            PresentedFrame::Inline(inline_chain[index])
        });

    Ok(FramePresentation {
        instruction,
        frame,
        hidden_inline_frames,
    })
}

fn presentation_visible_count(
    location: &ImageLocation,
    presentation: &FramePresentation,
) -> Result<usize> {
    if matches!(presentation.frame, PresentedFrame::Ambiguous(_)) {
        return Err(Error::AmbiguousInlineFrame);
    }
    let InlineFrameLookup::Unique(chain) = &location.inline_frames else {
        return match presentation.frame {
            PresentedFrame::Physical => Ok(0),
            PresentedFrame::Inline(_) | PresentedFrame::Ambiguous(_) => {
                Err(Error::LocationUnavailable)
            }
        };
    };
    let visible = match presentation.frame {
        PresentedFrame::Physical => 0,
        PresentedFrame::Inline(selected) => chain
            .instances
            .iter()
            .position(|instance| *instance == selected)
            .map(|index| index + 1)
            .ok_or(Error::LocationUnavailable)?,
        PresentedFrame::Ambiguous(_) => unreachable!("rejected above"),
    };
    let hidden =
        u32::try_from(chain.instances.len() - visible).map_err(|_| Error::LocationUnavailable)?;
    if hidden != presentation.hidden_inline_frames {
        return Err(Error::LocationUnavailable);
    }

    Ok(visible)
}

fn apply_presentation(
    module_image: &ModuleImage,
    location: &mut ImageLocation,
    presentation: &FramePresentation,
) -> Result<()> {
    let visible = presentation_visible_count(location, presentation)?;
    let InlineFrameLookup::Unique(chain) = &location.inline_frames else {
        return Ok(());
    };
    let selected_instance = visible
        .checked_sub(1)
        .and_then(|index| chain.instances.get(index).copied())
        .or(location.physical_instance);
    location.function = selected_instance
        .and_then(|instance| module_image.code_instance(instance))
        .and_then(|instance| module_image.function(instance.function))
        .cloned();
    location.source = if visible < chain.instances.len() {
        chain
            .instances
            .get(visible)
            .and_then(|instance| module_image.code_instance(*instance))
            .and_then(|instance| match &instance.kind {
                CodeInstanceKind::Inline { call_site } => call_site.clone(),
                CodeInstanceKind::OutOfLine => None,
            })
    } else {
        location.source.clone()
    };

    Ok(())
}

fn selected_code_instance(
    location: &ImageLocation,
    presentation: &FramePresentation,
) -> Result<Option<CodeInstanceId>> {
    presentation_visible_count(location, presentation)?;

    Ok(match presentation.frame {
        PresentedFrame::Physical => location.physical_instance,
        PresentedFrame::Inline(instance) => Some(instance),
        PresentedFrame::Ambiguous(_) => return Err(Error::AmbiguousInlineFrame),
    })
}

fn source_for_code_instance(
    module_image: &ModuleImage,
    location: &ImageLocation,
    selected: CodeInstanceId,
) -> Option<SourceLocation> {
    let InlineFrameLookup::Unique(chain) = &location.inline_frames else {
        return (location.physical_instance == Some(selected))
            .then(|| location.source.clone())
            .flatten();
    };
    let visible = if location.physical_instance == Some(selected) {
        0
    } else {
        chain
            .instances
            .iter()
            .position(|instance| *instance == selected)?
            + 1
    };

    if visible < chain.instances.len() {
        chain
            .instances
            .get(visible)
            .and_then(|instance| module_image.code_instance(*instance))
            .and_then(|instance| match &instance.kind {
                CodeInstanceKind::Inline { call_site } => call_site.clone(),
                CodeInstanceKind::OutOfLine => None,
            })
    } else {
        location.source.clone()
    }
}

fn source_line_changed(start: Option<&SourceLocation>, current: Option<&SourceLocation>) -> bool {
    current.is_some_and(|current| {
        start.is_none_or(|start| start.file != current.file || start.line != current.line)
    })
}

fn expand_inline_backtrace(
    physical: Backtrace,
    module_image: &ModuleImage,
    loaded_module: LoadedModule,
    presentation: &FramePresentation,
) -> Result<Backtrace> {
    let mut frames = Vec::new();

    for physical_frame in physical.frames.iter() {
        let context = FrameContext {
            instruction: physical_frame.instruction,
            cfa: None,
            signal_frame: physical_frame.kind == FrameKind::Signal,
        };
        let location = frame_lookup_address(physical_frame.level, &context)
            .and_then(|address| loaded_module.image_address(address).ok())
            .filter(|address| module_image.contains_address(*address))
            .map(|address| module_image.locate(address));
        let module = location.as_ref().map(|_| loaded_module.id);
        let physical_source = if let Some(location) = &location
            && let InlineFrameLookup::Unique(chain) = &location.inline_frames
        {
            let visible = if physical_frame.level == 0 {
                presentation_visible_count(location, presentation)?
            } else {
                chain.instances.len()
            };
            let mut source = if visible < chain.instances.len() {
                chain
                    .instances
                    .get(visible)
                    .and_then(|instance| module_image.code_instance(*instance))
                    .and_then(|instance| match &instance.kind {
                        CodeInstanceKind::Inline { call_site } => call_site.clone(),
                        CodeInstanceKind::OutOfLine => None,
                    })
            } else {
                location.source.clone()
            };

            for &instance_id in chain.instances[..visible].iter().rev() {
                let instance = module_image
                    .code_instance(instance_id)
                    .expect("inline chain references a known instance");
                let function = module_image.function(instance.function).cloned();
                let level = u32::try_from(frames.len()).expect("frame count fits in u32");

                frames.push(StackFrame::from_parts(
                    level,
                    FrameKind::Inline,
                    module,
                    physical_frame.instruction,
                    FrameMetadata {
                        code_instance: Some(instance.id),
                        function,
                        source,
                    },
                ));
                source = match &instance.kind {
                    CodeInstanceKind::Inline { call_site } => call_site.clone(),
                    CodeInstanceKind::OutOfLine => None,
                };
            }
            source
        } else {
            location
                .as_ref()
                .and_then(|location| location.source.clone())
        };

        let physical_instance = location
            .as_ref()
            .and_then(|location| location.physical_instance)
            .and_then(|instance| module_image.code_instance(instance));
        let function = physical_instance
            .and_then(|instance| module_image.function(instance.function))
            .cloned();
        let level = u32::try_from(frames.len()).expect("frame count fits in u32");

        frames.push(StackFrame::from_parts(
            level,
            physical_frame.kind,
            module,
            physical_frame.instruction,
            FrameMetadata {
                code_instance: physical_instance.map(|instance| instance.id),
                function,
                source: physical_source,
            },
        ));
    }

    Ok(Backtrace {
        thread: physical.thread,
        frames: frames.into(),
        termination: physical.termination,
    })
}

struct PtraceMemory<'a> {
    ptrace: &'a dyn LinuxTraceOps,
    pid: Pid,
}

struct LinuxVariableRuntime<'a, P> {
    ptrace: &'a P,
    pid: Pid,
    loaded_module: LoadedModule,
    breakpoints: &'a BTreeMap<VirtualAddress, BreakpointSite>,
    native: &'a libc::user_regs_struct,
    floating: Option<std::result::Result<libc::user_fpregs_struct, Arc<str>>>,
    cfa: std::result::Result<VirtualAddress, VariableUnavailableReason>,
}

impl<P: LinuxTraceOps> VariableRuntime for LinuxVariableRuntime<'_, P> {
    fn register(
        &mut self,
        register: u16,
    ) -> std::result::Result<VariableRegister, VariableUnavailableReason> {
        if let Some(value) = x86_64_general_variable_register(self.native, register) {
            return Ok(value);
        }
        if (17..=32).contains(&register) {
            let floating = self.floating.get_or_insert_with(|| {
                self.ptrace
                    .floating_registers(self.pid)
                    .map_err(|error| Arc::from(error.to_string()))
            });
            return floating
                .as_ref()
                .map_err(|error| {
                    VariableUnavailableReason::RegisterUnavailable(
                        format!("xmm{} ({error})", register - 17).into(),
                    )
                })
                .map(|floating| x86_64_xmm_variable_register(floating, register));
        }
        Err(crate::UnsupportedVariableFeature::RegisterClass.into())
    }

    fn call_frame_cfa(&self) -> std::result::Result<VirtualAddress, VariableUnavailableReason> {
        self.cfa.clone()
    }

    fn relocate(&self, address: ImageAddress) -> std::result::Result<VirtualAddress, Arc<str>> {
        self.loaded_module
            .virtual_address(address)
            .map_err(|error| error.to_string().into())
    }

    fn read_memory(
        &mut self,
        address: VirtualAddress,
        size: usize,
    ) -> std::result::Result<Arc<[u8]>, Arc<str>> {
        read_logical_memory(self.ptrace, self.pid, self.breakpoints, address, size)
            .map(Arc::from)
            .map_err(|error| error.to_string().into())
    }
}

fn read_logical_memory(
    ptrace: &impl LinuxTraceOps,
    pid: Pid,
    breakpoints: &BTreeMap<VirtualAddress, BreakpointSite>,
    address: VirtualAddress,
    size: usize,
) -> Result<Vec<u8>> {
    read_logical_memory_with(address, size, breakpoints, |current| {
        ptrace.read_word(pid, current)
    })
}

fn read_logical_memory_with(
    address: VirtualAddress,
    size: usize,
    breakpoints: &BTreeMap<VirtualAddress, BreakpointSite>,
    mut read_word: impl FnMut(u64) -> Result<u64>,
) -> Result<Vec<u8>> {
    if size > MAX_LOGICAL_MEMORY_READ {
        return Err(backend_error(LinuxError::MemoryReadTooLarge {
            size,
            maximum: MAX_LOGICAL_MEMORY_READ,
        }));
    }
    let end = address
        .get()
        .checked_add(u64::try_from(size).expect("memory read size fits u64"))
        .ok_or(Error::AddressOverflow)?;
    let mut bytes = Vec::with_capacity(size);
    let mut current = address.get();
    while current < end {
        let mut word = read_word(current)?.to_le_bytes();
        for (&site_address, site) in breakpoints {
            let Some(offset) = site_address.get().checked_sub(current) else {
                continue;
            };
            if site.installed && offset < word.len() as u64 {
                word[usize::try_from(offset).expect("word offset fits usize")] = site.original_byte;
            }
        }
        let remaining = usize::try_from(end - current).expect("remaining bytes fit usize");
        let count = remaining.min(word.len());
        bytes.extend_from_slice(&word[..count]);
        current = current
            .checked_add(u64::try_from(count).expect("word size fits u64"))
            .ok_or(Error::AddressOverflow)?;
    }
    Ok(bytes)
}

impl MemoryReader for PtraceMemory<'_> {
    fn read_u64(&mut self, address: VirtualAddress) -> std::result::Result<u64, ()> {
        self.ptrace
            .read_word(self.pid, address.get())
            .map_err(|_| ())
    }
}

struct DwarfCallerProvider<'a> {
    unwind_info: &'a dyn UnwindInfo,
    loaded_module: LoadedModule,
    module_image: &'a ModuleImage,
    registers: RegisterFile,
    memory: PtraceMemory<'a>,
    first: bool,
}

impl CallerProvider for DwarfCallerProvider<'_> {
    fn caller(&mut self, current: &FrameContext) -> CallerResult {
        let lookup = if self.first || current.signal_frame {
            current.instruction
        } else {
            let Some(address) = current.instruction.get().checked_sub(1) else {
                return CallerResult::Finished(UnwindTermination::Complete);
            };
            VirtualAddress::new(address)
        };
        self.first = false;
        let Ok(image_address) = self.loaded_module.image_address(lookup) else {
            return CallerResult::Finished(UnwindTermination::ModuleNotFound { address: lookup });
        };
        if !self.module_image.contains_address(image_address) {
            return CallerResult::Finished(UnwindTermination::ModuleNotFound { address: lookup });
        }
        let step = match self
            .unwind_info
            .unwind(image_address, &self.registers, &mut self.memory)
        {
            Ok(step) => step,
            Err(mut termination) => {
                if let UnwindTermination::NoUnwindInfo { address } = &mut termination {
                    *address = lookup;
                }
                return CallerResult::Finished(termination);
            }
        };
        let Some(instruction) = step.registers.get(16) else {
            return CallerResult::Finished(UnwindTermination::Complete);
        };
        if instruction == 0 {
            return CallerResult::Finished(UnwindTermination::Complete);
        }
        if step.cfa.get() == current.cfa.map_or(0, VirtualAddress::get)
            && instruction == current.instruction.get()
        {
            return CallerResult::Finished(UnwindTermination::InvalidCaller {
                description: "caller did not make progress".into(),
            });
        }

        self.registers = step.registers;
        CallerResult::Caller(FrameContext {
            instruction: VirtualAddress::new(instruction),
            cfa: Some(step.cfa),
            signal_frame: step.signal_frame,
        })
    }
}

fn x86_64_registers(registers: &libc::user_regs_struct) -> RegisterFile {
    RegisterFile::new([
        (0, registers.rax),
        (1, registers.rdx),
        (2, registers.rcx),
        (3, registers.rbx),
        (4, registers.rsi),
        (5, registers.rdi),
        (6, registers.rbp),
        (7, registers.rsp),
        (8, registers.r8),
        (9, registers.r9),
        (10, registers.r10),
        (11, registers.r11),
        (12, registers.r12),
        (13, registers.r13),
        (14, registers.r14),
        (15, registers.r15),
        (16, registers.rip),
        (49, registers.eflags),
    ])
}

fn x86_64_general_variable_register(
    registers: &libc::user_regs_struct,
    dwarf: u16,
) -> Option<VariableRegister> {
    let descriptor = x86_64_general_register_descriptor(dwarf)?;
    let value = match dwarf {
        0 => registers.rax,
        1 => registers.rdx,
        2 => registers.rcx,
        3 => registers.rbx,
        4 => registers.rsi,
        5 => registers.rdi,
        6 => registers.rbp,
        7 => registers.rsp,
        8 => registers.r8,
        9 => registers.r9,
        10 => registers.r10,
        11 => registers.r11,
        12 => registers.r12,
        13 => registers.r13,
        14 => registers.r14,
        15 => registers.r15,
        16 => registers.rip,
        49 => registers.eflags,
        _ => return None,
    };
    Some(VariableRegister {
        descriptor,
        bytes: Arc::from(value.to_le_bytes()),
    })
}

fn x86_64_general_register_descriptor(dwarf: u16) -> Option<RegisterDescriptor> {
    let (id, name, role) = match dwarf {
        0 => (0, "rax", None),
        1 => (3, "rdx", None),
        2 => (2, "rcx", None),
        3 => (1, "rbx", None),
        4 => (4, "rsi", None),
        5 => (5, "rdi", None),
        6 => (6, "rbp", Some(RegisterRole::FramePointer)),
        7 => (7, "rsp", Some(RegisterRole::StackPointer)),
        8 => (8, "r8", None),
        9 => (9, "r9", None),
        10 => (10, "r10", None),
        11 => (11, "r11", None),
        12 => (12, "r12", None),
        13 => (13, "r13", None),
        14 => (14, "r14", None),
        15 => (15, "r15", None),
        16 => (16, "rip", Some(RegisterRole::ProgramCounter)),
        49 => (17, "rflags", None),
        _ => return None,
    };
    Some(RegisterDescriptor {
        id: RegisterId::new(id),
        name: name.into(),
        bits: 64,
        role,
    })
}

fn x86_64_xmm_variable_register(
    registers: &libc::user_fpregs_struct,
    dwarf: u16,
) -> VariableRegister {
    let index = usize::from(dwarf - 17);
    let mut bytes = Vec::with_capacity(16);
    for word in &registers.xmm_space[index * 4..index * 4 + 4] {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    VariableRegister {
        descriptor: RegisterDescriptor {
            id: RegisterId::new(27 + u32::try_from(index).expect("XMM index fits u32")),
            name: format!("xmm{index}").into(),
            bits: 128,
            role: None,
        },
        bytes: bytes.into(),
    }
}

fn x86_64_register_snapshot(
    revision: u64,
    pid: Pid,
    target: crate::TargetDescription,
    native: &libc::user_regs_struct,
) -> RegisterSnapshot {
    let general = [
        (0, native.rax),
        (3, native.rbx),
        (2, native.rcx),
        (1, native.rdx),
        (4, native.rsi),
        (5, native.rdi),
        (6, native.rbp),
        (7, native.rsp),
        (8, native.r8),
        (9, native.r9),
        (10, native.r10),
        (11, native.r11),
        (12, native.r12),
        (13, native.r13),
        (14, native.r14),
        (15, native.r15),
        (16, native.rip),
        (49, native.eflags),
    ];
    let special = [
        ("cs", 16, None, native.cs),
        ("ss", 16, None, native.ss),
        ("ds", 16, None, native.ds),
        ("es", 16, None, native.es),
        ("fs", 16, None, native.fs),
        ("gs", 16, None, native.gs),
        ("fs_base", 64, None, native.fs_base),
        ("gs_base", 64, None, native.gs_base),
        ("orig_rax", 64, None, native.orig_rax),
    ];
    let registers = general
        .into_iter()
        .map(|(dwarf, value)| RegisterValue {
            register: x86_64_general_register_descriptor(dwarf)
                .expect("snapshot uses supported DWARF registers"),
            bytes: Arc::from(value.to_le_bytes()),
        })
        .chain(
            special
                .into_iter()
                .enumerate()
                .map(|(offset, (name, bits, role, value))| {
                    let bytes = value.to_le_bytes();
                    let byte_count = usize::from(bits / 8);
                    RegisterValue {
                        register: RegisterDescriptor {
                            id: RegisterId::new(
                                18 + u32::try_from(offset).expect("x86-64 register ID fits u32"),
                            ),
                            name: name.into(),
                            bits,
                            role,
                        },
                        bytes: Arc::from(&bytes[..byte_count]),
                    }
                }),
        )
        .collect::<Vec<_>>()
        .into();
    RegisterSnapshot {
        revision,
        thread: debug_thread_id(pid),
        target,
        registers,
    }
}

trait LinuxTraceOps {
    fn spawn(&self, executable: &Path) -> Result<Pid>;
    fn spawn_waiter(&self, messages: mpsc::Sender<ControllerMessage>) -> Result<JoinHandle<()>>;
    fn kill(&self, pid: Pid, signal: NixSignal) -> Result<()>;
    fn reap(&self, pid: Pid) -> Result<()>;
    fn thread_group_id(&self, pid: Pid) -> Result<Pid>;
    fn load_bias(&self, pid: Pid, executable: &Path) -> Result<u64>;
    fn read_word(&self, pid: Pid, address: u64) -> Result<u64>;
    fn write_word(&self, pid: Pid, address: u64, value: u64) -> Result<()>;
    fn continue_execution(&self, pid: Pid, signal: Option<NixSignal>) -> Result<()>;
    fn continue_during_shutdown(&self, pid: Pid) -> Result<()>;
    fn step(&self, pid: Pid, signal: Option<NixSignal>) -> Result<()>;
    fn registers(&self, pid: Pid) -> Result<libc::user_regs_struct>;
    fn floating_registers(&self, _pid: Pid) -> Result<libc::user_fpregs_struct> {
        Err(backend_error(LinuxError::UnsupportedFloatingRegisters))
    }
    fn set_registers(&self, pid: Pid, registers: libc::user_regs_struct) -> Result<()>;
    fn set_options(&self, pid: Pid) -> Result<()>;
    fn event_message(&self, pid: Pid) -> Result<libc::c_long>;
    fn signal_metadata(&self, pid: Pid) -> std::result::Result<SignalMetadata, Errno>;
    fn request_stop(&self, process: Pid, thread: Pid) -> Result<()>;
    fn install_breakpoint(
        &self,
        pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
        owner: BreakpointOwner,
    ) -> Result<()>;
    fn remove_breakpoint(
        &self,
        pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
    ) -> Result<()>;
    fn reinstall_breakpoint(
        &self,
        pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
    ) -> Result<()>;
}

struct LinuxPtrace {
    affinity: ThreadAffinity,
    not_send_or_sync: PhantomData<Rc<()>>,
}

impl LinuxPtrace {
    fn new() -> Self {
        Self {
            affinity: ThreadAffinity::new(),
            not_send_or_sync: PhantomData,
        }
    }

    fn assert_owner_thread(&self) {
        self.affinity.assert_owner();
    }
}

impl LinuxTraceOps for LinuxPtrace {
    fn spawn_waiter(&self, messages: mpsc::Sender<ControllerMessage>) -> Result<JoinHandle<()>> {
        self.assert_owner_thread();
        spawn_waiter(messages)
    }

    fn kill(&self, pid: Pid, signal: NixSignal) -> Result<()> {
        self.assert_owner_thread();
        match signal::kill(pid, signal) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(error) => Err(backend_error(LinuxError::System(error))),
        }
    }

    fn reap(&self, pid: Pid) -> Result<()> {
        self.assert_owner_thread();
        waitpid(pid, Some(WaitPidFlag::__WALL))
            .map(|_| ())
            .map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn thread_group_id(&self, pid: Pid) -> Result<Pid> {
        self.assert_owner_thread();
        thread_group_id(pid)
    }

    fn load_bias(&self, pid: Pid, executable: &Path) -> Result<u64> {
        self.assert_owner_thread();
        load_bias(pid, executable)
    }

    fn spawn(&self, executable: &Path) -> Result<Pid> {
        self.assert_owner_thread();
        let mut command = ProcessCommand::new(executable);
        trace_child(&mut command);
        let child = command.spawn()?;
        Ok(Pid::from_raw(
            i32::try_from(child.id()).map_err(|_| Error::AddressOverflow)?,
        ))
    }

    fn read_word(&self, pid: Pid, address: u64) -> Result<u64> {
        self.assert_owner_thread();
        let value = ptrace::read(pid, address as ptrace::AddressType)
            .map_err(|error| backend_error(LinuxError::System(error)))?;
        Ok(u64::from_ne_bytes(value.to_ne_bytes()))
    }

    fn write_word(&self, pid: Pid, address: u64, value: u64) -> Result<()> {
        self.assert_owner_thread();
        let value = libc::c_long::from_ne_bytes(value.to_ne_bytes());
        ptrace::write(pid, address as ptrace::AddressType, value)
            .map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn continue_execution(&self, pid: Pid, signal: Option<NixSignal>) -> Result<()> {
        self.assert_owner_thread();
        ptrace::cont(pid, signal).map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn continue_during_shutdown(&self, pid: Pid) -> Result<()> {
        self.assert_owner_thread();
        match ptrace::cont(pid, Some(NixSignal::SIGKILL)) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(error) => Err(backend_error(LinuxError::System(error))),
        }
    }

    fn step(&self, pid: Pid, signal: Option<NixSignal>) -> Result<()> {
        self.assert_owner_thread();
        ptrace::step(pid, signal).map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn registers(&self, pid: Pid) -> Result<libc::user_regs_struct> {
        self.assert_owner_thread();
        ptrace::getregs(pid).map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn floating_registers(&self, pid: Pid) -> Result<libc::user_fpregs_struct> {
        self.assert_owner_thread();
        ptrace::getregset::<ptrace::regset::NT_PRFPREG>(pid)
            .map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn set_registers(&self, pid: Pid, registers: libc::user_regs_struct) -> Result<()> {
        self.assert_owner_thread();
        ptrace::setregs(pid, registers).map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn set_options(&self, pid: Pid) -> Result<()> {
        self.assert_owner_thread();
        let options = Options::PTRACE_O_EXITKILL
            | Options::PTRACE_O_TRACECLONE
            | Options::PTRACE_O_TRACEEXEC
            | Options::PTRACE_O_TRACEEXIT
            | Options::PTRACE_O_TRACESYSGOOD;
        ptrace::setoptions(pid, options).map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn event_message(&self, pid: Pid) -> Result<libc::c_long> {
        self.assert_owner_thread();
        ptrace::getevent(pid).map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn signal_metadata(&self, pid: Pid) -> std::result::Result<SignalMetadata, Errno> {
        self.assert_owner_thread();
        ptrace::getsiginfo(pid).map(|info| signal_metadata(&info))
    }

    fn request_stop(&self, process: Pid, thread: Pid) -> Result<()> {
        self.assert_owner_thread();
        tgkill(process, thread, NixSignal::SIGSTOP)
    }

    fn install_breakpoint(
        &self,
        pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
        owner: BreakpointOwner,
    ) -> Result<()> {
        self.assert_owner_thread();
        if let Some(site) = sites.get_mut(&address) {
            site.owners.insert(owner);
            return Ok(());
        }
        let word = self.read_word(pid, address.get())?;
        let original_byte = word.to_ne_bytes()[0];
        self.write_word(
            pid,
            address.get(),
            (word & !0xff) | u64::from(BREAKPOINT_OPCODE),
        )?;
        sites.insert(
            address,
            BreakpointSite {
                original_byte,
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
        self.assert_owner_thread();
        let site = sites.get_mut(&address).expect("known breakpoint site");
        if !site.installed {
            return Ok(());
        }
        let word = self.read_word(pid, address.get())?;
        self.write_word(
            pid,
            address.get(),
            (word & !0xff) | u64::from(site.original_byte),
        )?;
        site.installed = false;
        Ok(())
    }

    fn reinstall_breakpoint(
        &self,
        pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
    ) -> Result<()> {
        self.assert_owner_thread();
        let site = sites.get_mut(&address).expect("known breakpoint site");
        if site.installed {
            return Ok(());
        }
        let word = self.read_word(pid, address.get())?;
        self.write_word(
            pid,
            address.get(),
            (word & !0xff) | u64::from(BREAKPOINT_OPCODE),
        )?;
        site.installed = true;
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct ThreadAffinity {
    owner: ThreadId,
}

impl ThreadAffinity {
    fn new() -> Self {
        Self {
            owner: thread::current().id(),
        }
    }

    fn assert_owner(self) {
        assert_eq!(
            self.owner,
            thread::current().id(),
            "ptrace called from non-controller thread"
        );
    }
}

fn spawn_waiter(messages: mpsc::Sender<ControllerMessage>) -> Result<JoinHandle<()>> {
    Ok(thread::Builder::new()
        .name(WAITER_THREAD_NAME.into())
        .spawn(move || {
            loop {
                let status = match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::__WALL)) {
                    Ok(status) => status,
                    Err(Errno::EINTR) => continue,
                    Err(_) => break,
                };
                if messages
                    .blocking_send(ControllerMessage::Wait(status))
                    .is_err()
                {
                    break;
                }
            }
        })?)
}

#[allow(
    unsafe_code,
    reason = "pre_exec is the only way to establish child-side ptrace and parent-death behavior"
)]
fn trace_child(command: &mut ProcessCommand) {
    let expected_parent = std::process::id();

    // SAFETY: after fork, this closure invokes only async-signal-safe syscalls and
    // constructs fixed errno values before Command performs exec.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != i32::try_from(expected_parent).unwrap_or(i32::MAX) {
                return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
            }
            ptrace::traceme().map_err(|error| std::io::Error::from_raw_os_error(error as i32))
        });
    }
}

#[allow(
    unsafe_code,
    reason = "Linux exposes thread-directed signals through tgkill"
)]
fn tgkill(process: Pid, thread: Pid, signal: NixSignal) -> Result<()> {
    // SAFETY: tgkill takes three integer values and does not dereference user memory.
    let result = unsafe {
        libc::syscall(
            libc::SYS_tgkill,
            process.as_raw(),
            thread.as_raw(),
            signal as i32,
        )
    };
    if result == -1 {
        return Err(backend_error(LinuxError::System(Errno::last())));
    }
    Ok(())
}

#[allow(
    unsafe_code,
    reason = "libc exposes siginfo sender fields through union accessors"
)]
fn signal_metadata(info: &libc::siginfo_t) -> SignalMetadata {
    let code = info.si_code;
    let sender = if code <= 0 {
        // SAFETY: nonpositive si_code values use a siginfo layout containing si_pid.
        Some(unsafe { info.si_pid() })
    } else {
        None
    };
    SignalMetadata { code, sender }
}

const fn wait_status_pid(status: &WaitStatus) -> Option<Pid> {
    match *status {
        WaitStatus::Exited(pid, _)
        | WaitStatus::Signaled(pid, _, _)
        | WaitStatus::Stopped(pid, _)
        | WaitStatus::PtraceEvent(pid, _, _)
        | WaitStatus::PtraceSyscall(pid)
        | WaitStatus::Continued(pid) => Some(pid),
        WaitStatus::StillAlive => None,
    }
}

fn runtime_breakpoint_address(
    inferior: &Inferior,
    location: BreakpointLocation,
) -> Result<VirtualAddress> {
    match location {
        BreakpointLocation::Image(address) => inferior.loaded_module.virtual_address(address),
        BreakpointLocation::Virtual(address) => Ok(address),
    }
}

fn install_logical_breakpoint(
    ptrace: &dyn LinuxTraceOps,
    inferior: &mut Inferior,
    breakpoint: &Breakpoint,
) -> Result<()> {
    let owner = BreakpointOwner::User(breakpoint.id);
    let addresses = breakpoint
        .locations
        .iter()
        .map(|resolved| runtime_breakpoint_address(inferior, resolved.location))
        .collect::<Result<Vec<_>>>()?;
    let mut installed = Vec::with_capacity(addresses.len());

    for address in addresses {
        if let Err(cause) =
            ptrace.install_breakpoint(inferior.tgid, &mut inferior.breakpoints, address, owner)
        {
            for installed_address in installed.into_iter().rev() {
                if let Err(recovery) =
                    remove_breakpoint_owner_from(ptrace, inferior, installed_address, owner)
                {
                    return Err(backend_error(LinuxError::BreakpointInstallRecovery {
                        cause: cause.to_string(),
                        recovery: recovery.to_string(),
                    }));
                }
            }

            return Err(cause);
        }
        installed.push(address);
    }

    Ok(())
}

fn remove_logical_breakpoint(
    ptrace: &dyn LinuxTraceOps,
    inferior: &mut Inferior,
    breakpoint: &Breakpoint,
) -> Result<()> {
    let owner = BreakpointOwner::User(breakpoint.id);
    let addresses = breakpoint
        .locations
        .iter()
        .map(|resolved| runtime_breakpoint_address(inferior, resolved.location))
        .collect::<Result<Vec<_>>>()?;

    for &address in &addresses {
        let site = inferior
            .breakpoints
            .get(&address)
            .ok_or_else(|| backend_error(LinuxError::BreakpointSiteMissing(address)))?;
        if !site.owners.contains(&owner) {
            return Err(backend_error(LinuxError::BreakpointOwnerMissing(address)));
        }
    }

    let mut removed = Vec::new();
    for address in addresses {
        if let Err(cause) = remove_breakpoint_owner_from(ptrace, inferior, address, owner) {
            for prior in removed.into_iter().rev() {
                if let Err(recovery) = ptrace.install_breakpoint(
                    inferior.tgid,
                    &mut inferior.breakpoints,
                    prior,
                    owner,
                ) {
                    return Err(backend_error(LinuxError::BreakpointRemoveRecovery {
                        cause: cause.to_string(),
                        recovery: recovery.to_string(),
                    }));
                }
            }
            return Err(cause);
        }
        removed.push(address);
    }
    let removed_sites = removed
        .iter()
        .copied()
        .filter(|address| !inferior.breakpoints.contains_key(address))
        .collect::<BTreeSet<_>>();
    for thread in inferior.threads.values_mut() {
        if thread
            .stopped_at_breakpoint
            .is_some_and(|address| removed_sites.contains(&address))
        {
            // Breakpoint PCs are normalized when the trap is classified. With the
            // original instruction restored there is no repair step left to run.
            thread.stopped_at_breakpoint = None;
        }
    }
    Ok(())
}

fn validate_process(inferior: &Inferior, requested: ProcessId) -> Result<()> {
    if process_id(inferior.tgid) == requested {
        Ok(())
    } else {
        Err(Error::NotRunning)
    }
}

fn validate_public_stop(inferior: &Inferior, requested: Option<StopId>) -> Result<()> {
    match (&inferior.public_stop, requested) {
        (Some(current), Some(requested)) if current.id == requested => Ok(()),
        (Some(_), Some(_)) => Err(Error::StaleStop),
        (Some(_), None) => Ok(()),
        (None, _) => Err(Error::NotStopped),
    }
}

fn validate_stopped_thread(inferior: &Inferior, pid: Pid) -> Result<()> {
    let thread = inferior.threads.get(&pid).ok_or(Error::NotRunning)?;
    if matches!(thread.state, NativeThreadState::Stopped) {
        Ok(())
    } else {
        Err(Error::NotStopped)
    }
}

fn scoped_threads(inferior: &Inferior, scope: ResumeScope) -> Result<BTreeSet<Pid>> {
    match scope {
        ResumeScope::Process(requested) => {
            validate_process(inferior, requested)?;
            Ok(inferior.threads.keys().copied().collect())
        }
        ResumeScope::Thread(thread) => {
            let pid = debug_pid(thread);
            validate_stopped_thread(inferior, pid)?;
            Ok(BTreeSet::from([pid]))
        }
    }
}

fn remove_breakpoint_owner_from(
    ptrace: &dyn LinuxTraceOps,
    inferior: &mut Inferior,
    address: VirtualAddress,
    owner: BreakpointOwner,
) -> Result<()> {
    let remove_site = {
        let site = inferior
            .breakpoints
            .get_mut(&address)
            .ok_or_else(|| backend_error(LinuxError::BreakpointSiteMissing(address)))?;
        if !site.owners.contains(&owner) {
            return Err(backend_error(LinuxError::BreakpointOwnerMissing(address)));
        }
        site.owners.len() == 1
    };

    if remove_site {
        ptrace.remove_breakpoint(inferior.tgid, &mut inferior.breakpoints, address)?;
        let removed = inferior.breakpoints.remove(&address);
        assert!(removed.is_some(), "empty breakpoint site existed");
    } else {
        let removed = inferior
            .breakpoints
            .get_mut(&address)
            .expect("known breakpoint site")
            .owners
            .remove(&owner);
        assert!(removed, "known breakpoint owner existed");
    }
    Ok(())
}

fn collect_repairs(inferior: &Inferior) -> VecDeque<RepairGroup> {
    let active = inferior.active.as_ref().expect("execution is active");
    let mut grouped: BTreeMap<VirtualAddress, VecDeque<Pid>> = BTreeMap::new();
    for &pid in &active.resume_threads {
        if let Some(address) = inferior
            .threads
            .get(&pid)
            .and_then(|thread| thread.stopped_at_breakpoint)
        {
            grouped.entry(address).or_default().push_back(pid);
        }
    }
    grouped
        .into_iter()
        .map(|(address, remaining)| RepairGroup {
            address,
            remaining,
            current: None,
            site_removed: false,
        })
        .collect()
}

const fn is_stopping_signal(signal: NixSignal) -> bool {
    matches!(
        signal,
        NixSignal::SIGSTOP | NixSignal::SIGTSTP | NixSignal::SIGTTIN | NixSignal::SIGTTOU
    )
}

fn classify_stop_evidence(
    signal: NixSignal,
    status: String,
    siginfo: std::result::Result<SignalMetadata, Errno>,
    expected: &ExpectedStop,
    starting: bool,
    debugger_requested: bool,
    breakpoint: Option<VirtualAddress>,
) -> ClassifiedStop {
    if signal == NixSignal::SIGSTOP && starting {
        return ClassifiedStop::ThreadStart;
    }
    if debugger_requested {
        return ClassifiedStop::DebuggerRequested;
    }
    if signal == NixSignal::SIGTRAP
        && siginfo.as_ref().is_ok_and(is_single_step_trap)
        && matches!(
            expected,
            ExpectedStop::BreakpointRepair { .. } | ExpectedStop::UserStep { .. }
        )
    {
        return ClassifiedStop::Trace;
    }
    if let Some(address) = breakpoint {
        return ClassifiedStop::Breakpoint(address);
    }

    match siginfo {
        Ok(metadata) if signal != NixSignal::SIGTRAP || metadata.code <= 0 => {
            ClassifiedStop::SignalDelivery(PendingSignal {
                signal,
                code: metadata.code,
                sender: metadata.sender,
            })
        }
        Err(Errno::EINVAL) if is_stopping_signal(signal) => ClassifiedStop::GroupStop(signal),
        outcome => ClassifiedStop::Unclassifiable(RawStopRecord {
            status,
            siginfo: outcome,
        }),
    }
}

const fn is_single_step_trap(metadata: &SignalMetadata) -> bool {
    metadata.code == libc::TRAP_TRACE || metadata.code == TRAP_UNKNOWN
}

fn format_raw_stop(raw: &RawStopRecord) -> String {
    match raw.siginfo {
        Ok(metadata) => format!(
            "{}; siginfo code={} sender={:?}",
            raw.status, metadata.code, metadata.sender
        ),
        Err(error) => format!("{}; PTRACE_GETSIGINFO failed: {error}", raw.status),
    }
}

fn thread_group_id(pid: Pid) -> Result<Pid> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix("Tgid:"))
        .map(str::trim)
        .ok_or_else(|| {
            backend_error(LinuxError::UnexpectedWait(format!(
                "missing Tgid for {pid}"
            )))
        })?;
    let tgid = value.parse::<i32>().map_err(|_| {
        backend_error(LinuxError::UnexpectedWait(format!(
            "invalid Tgid for {pid}"
        )))
    })?;
    Ok(Pid::from_raw(tgid))
}

fn load_bias(pid: Pid, executable: &Path) -> Result<u64> {
    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps"))?;
    let data = std::fs::read(executable)?;
    let object = object::File::parse(data.as_slice())
        .map_err(|error| Error::backend(LinuxError::Object(error)))?;
    let image_base = object
        .segments()
        .map(|segment| segment.address())
        .min()
        .unwrap_or(0);
    let executable = executable.to_string_lossy();

    for line in maps.lines() {
        let mut fields = line.split_whitespace();
        let Some(range) = fields.next() else { continue };
        let _permissions = fields.next();
        let Some(offset) = fields.next() else {
            continue;
        };
        let _device = fields.next();
        let _inode = fields.next();
        let Some(path) = fields.next() else { continue };
        if path != executable || offset != "00000000" {
            continue;
        }
        let Some(start) = range.split('-').next() else {
            continue;
        };
        let mapping_start = u64::from_str_radix(start, 16)
            .map_err(|_| backend_error(LinuxError::LoadBias(executable.as_ref().into())))?;
        return mapping_start
            .checked_sub(image_base)
            .ok_or_else(|| backend_error(LinuxError::LoadBias(executable.as_ref().into())));
    }

    Err(backend_error(LinuxError::LoadBias(
        executable.as_ref().into(),
    )))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::{AddressRange, ImageAddress};

    #[test]
    fn logical_memory_reads_unaligned_cross_word_ranges_and_hides_traps() {
        let address = VirtualAddress::new(0x1003);
        let mut breakpoints = BTreeMap::new();
        breakpoints.insert(
            VirtualAddress::new(0x1005),
            BreakpointSite {
                original_byte: 0x55,
                installed: true,
                owners: BTreeSet::new(),
            },
        );
        let bytes = read_logical_memory_with(address, 10, &breakpoints, |current| {
            let mut bytes = [0_u8; 8];
            for (offset, byte) in bytes.iter_mut().enumerate() {
                *byte = u8::try_from(current + offset as u64 - 0x1000).expect("test byte fits u8");
            }
            if current <= 0x1005 && 0x1005 < current + 8 {
                bytes[usize::try_from(0x1005 - current).expect("test offset fits usize")] =
                    BREAKPOINT_OPCODE;
            }
            Ok(u64::from_le_bytes(bytes))
        })
        .expect("logical memory read");
        assert_eq!(bytes, [3, 4, 0x55, 6, 7, 8, 9, 10, 11, 12]);
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

    impl LinuxTraceOps for RecordingTrace {
        fn spawn(&self, _executable: &Path) -> Result<Pid> {
            self.record("spawn");
            Ok(self.pid)
        }

        fn spawn_waiter(
            &self,
            _messages: mpsc::Sender<ControllerMessage>,
        ) -> Result<JoinHandle<()>> {
            self.record("spawn_waiter");
            Ok(thread::spawn(|| {}))
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

        fn load_bias(&self, pid: Pid, _executable: &Path) -> Result<u64> {
            assert_eq!(pid, self.pid);
            self.record("load_bias");
            Ok(0x5000)
        }

        fn read_word(&self, _pid: Pid, _address: u64) -> Result<u64> {
            Self::unexpected("read_word")
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

        fn registers(&self, _pid: Pid) -> Result<libc::user_regs_struct> {
            Self::unexpected("registers")
        }

        fn set_registers(&self, _pid: Pid, _registers: libc::user_regs_struct) -> Result<()> {
            Self::unexpected("set_registers")
        }

        fn set_options(&self, pid: Pid) -> Result<()> {
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

    struct UnusedUnwindInfo;

    impl UnwindInfo for UnusedUnwindInfo {
        fn cfa(
            &self,
            _address: ImageAddress,
            _registers: &RegisterFile,
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
            _runtime: &mut dyn VariableRuntime,
        ) -> Result<Vec<crate::Variable>> {
            panic!("unexpected variable lookup")
        }
    }

    #[test]
    fn controller_lifecycle_is_driven_through_the_linux_effect_boundary() {
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
                source_files: Vec::new(),
                statements: Vec::new(),
                lines: Vec::new(),
            },
        ));
        let (message_sender, messages) = mpsc::channel(8);
        let (events, _) = broadcast::channel(8);
        let mut controller = Controller::new(
            SessionLease::acquire().expect("acquire test session"),
            Arc::new(PathBuf::from("/test/program")),
            image,
            Arc::new(UnusedUnwindInfo),
            Arc::new(UnusedVariableInfo),
            ControllerChannels {
                messages,
                message_sender,
                events,
            },
            trace,
        );
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
        assert!(!controller.handle_shutdown_wait(WaitStatus::Signaled(
            pid,
            NixSignal::SIGKILL,
            false,
        )));
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
    fn virtual_inline_step_emits_a_new_stop_without_native_operations() {
        let VirtualStepHarness {
            mut controller,
            mut events,
            actions,
            pid,
        } = virtual_step_controller();

        let execution = controller
            .try_virtual_step(process_id(pid), StopId::new(1), pid, StepKind::IntoSource)
            .expect("virtual step")
            .expect("hidden child exists");

        assert_eq!(execution, ExecutionId::new(2));
        assert!(actions.borrow().is_empty());
        let emitted = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        assert!(emitted.iter().any(|event| matches!(
            event,
            DebuggerEvent::InferiorStopped {
                execution_id: Some(execution_id),
                ..
            } if *execution_id == ExecutionId::new(2)
        )));
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
        assert_eq!(stop.id, StopId::new(2));
        assert_eq!(
            stop.presentations.get(&pid),
            Some(&FramePresentation {
                instruction: VirtualAddress::new(0x10),
                frame: PresentedFrame::Inline(CodeInstanceId::new(1)),
                hidden_inline_frames: 1,
            })
        );
        assert!(matches!(
            controller
                .try_virtual_step(process_id(pid), StopId::new(1), pid, StepKind::IntoSource,),
            Err(Error::StaleStop)
        ));
        assert!(actions.borrow().is_empty());
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
                source_files: Vec::new(),
                statements: Vec::new(),
                lines: Vec::new(),
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
        let (message_sender, messages) = mpsc::channel(8);
        let (events, event_receiver) = broadcast::channel(8);
        let mut controller = Controller::new(
            SessionLease::detached(),
            Arc::new(PathBuf::from("/test/inline")),
            Arc::clone(&image),
            Arc::new(UnusedUnwindInfo),
            Arc::new(UnusedVariableInfo),
            ControllerChannels {
                messages,
                message_sender,
                events,
            },
            trace,
        );
        let presentation = FramePresentation {
            instruction: VirtualAddress::new(0x10),
            frame: PresentedFrame::Physical,
            hidden_inline_frames: 2,
        };
        controller.inferior = Some(Inferior {
            tgid: pid,
            loaded_module: LoadedModule::main(image.id(), 0),
            breakpoints: BTreeMap::new(),
            threads: BTreeMap::from([(
                pid,
                TraceThread {
                    state: NativeThreadState::Stopped,
                    expected: ExpectedStop::None,
                    pending_signal: None,
                    reason: Some(StopReason::Pause),
                    stopped_at_breakpoint: None,
                    awaiting_breakpoint: None,
                    debugger_stop_pending: false,
                },
            )]),
            retired_threads: BTreeSet::new(),
            unowned_stops: BTreeMap::new(),
            waiter: None,
            active: None,
            repairs: VecDeque::new(),
            barrier: None,
            public_stop: Some(PublicStop {
                id: StopId::new(1),
                triggering_thread: pid,
                reason: StopReason::Pause,
                presentations: BTreeMap::from([(pid, presentation)]),
            }),
            selected_thread: Some(pid),
            next_execution: 1,
            next_stop: 1,
            next_barrier: 0,
            exec_unsupported: false,
        });

        VirtualStepHarness {
            controller,
            events: event_receiver,
            actions,
            pid,
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
            ClassifiedStop::Trace
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
}
