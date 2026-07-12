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
use crate::debug_info::UnwindInfo;
use crate::protocol::{
    DebuggerEvent, ExceptionDisposition, ExceptionInfo, ExecutionId, ExitStatus, InferiorState,
    ProcessId, Reply, Request, ResumeScope, StateSnapshot, StepKind, StopId, StopReason,
    ThreadSnapshot, ThreadState as ObservableThreadState,
};
use crate::unwind::{
    CallerProvider, CallerResult, DEFAULT_MAX_FRAMES, FrameContext, MemoryReader, RegisterFile,
    collect_backtrace,
};
use crate::{
    Backtrace, BreakpointLocation, Error, FrameKind, FunctionId, ImageLocation, LoadedModule,
    ModuleImage, RegisterDescriptor, RegisterId, RegisterRole, RegisterSnapshot, RegisterValue,
    Result, SourceLocation, StackFrame, ThreadId as DebugThreadId, UnwindTermination,
    VirtualAddress,
};

const CONTROLLER_THREAD_NAME: &str = "uscope-controller";
const WAITER_THREAD_NAME: &str = "uscope-waitpid";
const BREAKPOINT_OPCODE: u8 = 0xcc;
const TRAP_UNKNOWN: i32 = 5;

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

struct SessionLease;

impl SessionLease {
    fn acquire() -> Result<Self> {
        LINUX_SESSION_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| backend_error(LinuxError::SessionActive))?;
        Ok(Self)
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
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
    User,
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
    function: Option<FunctionId>,
    stack_pointer: u64,
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
    #[error("the inferior replaced its executable image; loading the new image is not supported")]
    UnsupportedExec,
    #[error("could not determine the caller frame for step out: {0:?}")]
    CallerUnavailable(UnwindTermination),
    #[error("breakpoint site {0:?} was not found")]
    BreakpointSiteMissing(VirtualAddress),
    #[error("breakpoint site {0:?} did not have the expected owner")]
    BreakpointOwnerMissing(VirtualAddress),
    #[error("resume failed ({cause}) and recovery also failed ({recovery})")]
    ResumeRecovery { cause: String, recovery: String },
}

struct Controller {
    _lease: SessionLease,
    executable: Arc<PathBuf>,
    module_image: Arc<ModuleImage>,
    unwind_info: Arc<dyn UnwindInfo>,
    messages: mpsc::Receiver<ControllerMessage>,
    message_sender: mpsc::Sender<ControllerMessage>,
    events: broadcast::Sender<DebuggerEvent>,
    ptrace: LinuxPtrace,
    inferior: Option<Inferior>,
    pending_breakpoints: Vec<BreakpointLocation>,
    launch_reply: Option<Reply<ExecutionId>>,
    shutdown_reply: Option<Reply<()>>,
    revision: u64,
}

pub fn spawn_controller(
    executable: Arc<PathBuf>,
    module_image: Arc<ModuleImage>,
    unwind_info: Arc<dyn UnwindInfo>,
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
                messages,
                message_sender,
                events,
            )
            .run();
        })?)
}

impl Controller {
    fn new(
        lease: SessionLease,
        executable: Arc<PathBuf>,
        module_image: Arc<ModuleImage>,
        unwind_info: Arc<dyn UnwindInfo>,
        messages: mpsc::Receiver<ControllerMessage>,
        message_sender: mpsc::Sender<ControllerMessage>,
        events: broadcast::Sender<DebuggerEvent>,
    ) -> Self {
        Self {
            _lease: lease,
            executable,
            module_image,
            unwind_info,
            messages,
            message_sender,
            events,
            ptrace: LinuxPtrace::new(),
            inferior: None,
            pending_breakpoints: Vec::new(),
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

    fn handle_request(&mut self, request: Request) -> bool {
        match request {
            Request::AddBreakpoint { location, reply } => {
                let _ = reply.send(self.add_breakpoint(location));
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

impl Controller {
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

    fn add_breakpoint(&mut self, location: BreakpointLocation) -> Result<()> {
        if self.pending_breakpoints.contains(&location) {
            return Ok(());
        }

        if let Some(inferior) = self.inferior.as_mut() {
            validate_public_stop(inferior, inferior.public_stop.as_ref().map(|stop| stop.id))?;
            let address = runtime_breakpoint_address(inferior, location)?;
            self.ptrace.install_breakpoint(
                inferior.tgid,
                &mut inferior.breakpoints,
                address,
                BreakpointOwner::User,
            )?;
        }
        self.pending_breakpoints.push(location);

        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::BreakpointsChanged {
            revision: self.revision,
        });
        Ok(())
    }

    fn launch(&mut self, reply: Reply<ExecutionId>) {
        if self.inferior.is_some() || self.launch_reply.is_some() {
            let _ = reply.send(Err(Error::AlreadyRunning));
            return;
        }

        match self.ptrace.spawn(&self.executable) {
            Ok(pid) => {
                let waiter = match spawn_waiter(self.message_sender.clone()) {
                    Ok(waiter) => waiter,
                    Err(error) => {
                        let _ = signal::kill(pid, NixSignal::SIGKILL);
                        let _ = waitpid(pid, Some(WaitPidFlag::__WALL));
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
        let load_bias = load_bias(pid, &self.executable)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.loaded_module = LoadedModule::main(self.module_image.id(), load_bias);

        for &location in &self.pending_breakpoints {
            let address = runtime_breakpoint_address(inferior, location)?;
            self.ptrace.install_breakpoint(
                pid,
                &mut inferior.breakpoints,
                address,
                BreakpointOwner::User,
            )?;
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
                            let _ = signal::kill(inferior.tgid, NixSignal::SIGKILL);
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

impl Controller {
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

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let thread = inferior.threads.get_mut(&pid).ok_or(Error::NotRunning)?;
        let signal = thread.pending_signal.map(|pending| pending.signal);
        self.ptrace.step(pid, signal)?;
        thread.pending_signal = None;
        thread.expected = ExpectedStop::UserStep { kind };
        thread.state = NativeThreadState::Running;
        Ok(())
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
                .is_some_and(|site| site.owners.contains(&BreakpointOwner::User));
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

impl Controller {
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
        let location = self.image_location(VirtualAddress::new(registers.rip));
        let source = location
            .as_ref()
            .and_then(|location| location.source.clone());
        let function = location
            .as_ref()
            .and_then(|location| location.function.as_ref())
            .map(|function| function.id);
        let changed = source.is_some() && source != start.source;

        Ok(match kind {
            StepKind::Instruction => true,
            StepKind::IntoSource => changed,
            StepKind::OverSource => changed && registers.rsp >= start.stack_pointer,
            StepKind::Out => registers.rsp > start.stack_pointer && function != start.function,
        })
    }

    fn step_start(&self, pid: Pid, kind: StepKind) -> Result<StepStart> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let thread = inferior.threads.get(&pid).ok_or(Error::NotRunning)?;
        if !matches!(thread.state, NativeThreadState::Stopped) {
            return Err(Error::NotStopped);
        }
        let registers = self.ptrace.registers(pid)?;
        let location = self.image_location(VirtualAddress::new(registers.rip));
        let source = location
            .as_ref()
            .and_then(|location| location.source.clone());
        let function = location
            .as_ref()
            .and_then(|location| location.function.as_ref());
        let mut plan_addresses = BTreeSet::new();

        if kind == StepKind::Out {
            plan_addresses.insert(self.caller_address(pid, &registers)?);
        } else if kind == StepKind::OverSource
            && let (Some(source), Some(function)) = (&source, function)
        {
            let return_address = self.caller_address(pid, &registers)?;
            for line in self.module_image.line_entries() {
                if function.contains(line.range.start) && line.location != *source {
                    plan_addresses
                        .insert(inferior.loaded_module.virtual_address(line.range.start)?);
                }
            }
            plan_addresses.insert(return_address);
        }

        Ok(StepStart {
            source,
            function: function.map(|function| function.id),
            stack_pointer: registers.rsp,
            plan_addresses,
        })
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
        let child_tgid = thread_group_id(child)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        if child_tgid != inferior.tgid {
            let _ = signal::kill(child, NixSignal::SIGKILL);
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

impl Controller {
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
        let mut bytes = self.ptrace.read_word(pid, address.get())?.to_ne_bytes();
        for (&site_address, site) in &inferior.breakpoints {
            let Some(offset) = site_address.get().checked_sub(address.get()) else {
                continue;
            };
            if site.installed && offset < bytes.len() as u64 {
                bytes[usize::try_from(offset).expect("word offset fits usize")] =
                    site.original_byte;
            }
        }
        Ok(u64::from_ne_bytes(bytes))
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

    fn stopped_location(
        &self,
        stop_id: StopId,
        pid: Pid,
    ) -> Result<(LoadedModule, VirtualAddress)> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_stopped_thread(inferior, pid)?;
        let registers = self.ptrace.registers(pid)?;
        Ok((inferior.loaded_module, VirtualAddress::new(registers.rip)))
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
                breakpoints: self.pending_breakpoints.clone().into(),
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
            breakpoints: self.pending_breakpoints.clone().into(),
        }
    }

    fn backtrace(&self, stop_id: StopId, pid: Pid) -> Result<Backtrace> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_stopped_thread(inferior, pid)?;

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

        Ok(collect_backtrace(
            debug_thread_id(pid),
            initial,
            &mut provider,
            |level, context| {
                let location = loaded_module
                    .image_address(context.instruction)
                    .ok()
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
        ))
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

    fn select_thread(&mut self, stop_id: StopId, pid: Pid) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_stopped_thread(inferior, pid)?;
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
        match signal::kill(inferior.tgid, NixSignal::SIGKILL) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(error) => Err(backend_error(LinuxError::System(error))),
        }
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

struct PtraceMemory<'a> {
    ptrace: &'a LinuxPtrace,
    pid: Pid,
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

fn x86_64_register_snapshot(
    revision: u64,
    pid: Pid,
    target: crate::TargetDescription,
    native: &libc::user_regs_struct,
) -> RegisterSnapshot {
    let values = [
        ("rax", 64, None, native.rax),
        ("rbx", 64, None, native.rbx),
        ("rcx", 64, None, native.rcx),
        ("rdx", 64, None, native.rdx),
        ("rsi", 64, None, native.rsi),
        ("rdi", 64, None, native.rdi),
        ("rbp", 64, Some(RegisterRole::FramePointer), native.rbp),
        ("rsp", 64, Some(RegisterRole::StackPointer), native.rsp),
        ("r8", 64, None, native.r8),
        ("r9", 64, None, native.r9),
        ("r10", 64, None, native.r10),
        ("r11", 64, None, native.r11),
        ("r12", 64, None, native.r12),
        ("r13", 64, None, native.r13),
        ("r14", 64, None, native.r14),
        ("r15", 64, None, native.r15),
        ("rip", 64, Some(RegisterRole::ProgramCounter), native.rip),
        ("rflags", 64, None, native.eflags),
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
    let registers = values
        .into_iter()
        .enumerate()
        .map(|(id, (name, bits, role, value))| {
            let bytes = value.to_le_bytes();
            let byte_count = usize::from(bits / 8);
            RegisterValue {
                register: RegisterDescriptor {
                    id: RegisterId::new(u32::try_from(id).expect("x86-64 register ID fits u32")),
                    name: name.into(),
                    bits,
                    role,
                },
                bytes: Arc::from(&bytes[..byte_count]),
            }
        })
        .collect::<Vec<_>>()
        .into();
    RegisterSnapshot {
        revision,
        thread: debug_thread_id(pid),
        target,
        registers,
    }
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
    ptrace: &LinuxPtrace,
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
    use super::*;

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
