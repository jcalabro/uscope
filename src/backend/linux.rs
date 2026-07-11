use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::rc::Rc;
use std::sync::Arc;
use std::thread::{self, JoinHandle, ThreadId};

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
    DebuggerEvent, ExceptionInfo, ExitStatus, InferiorState, ProcessId, Reply, Request,
    StateSnapshot, StopReason,
};
use crate::unwind::{
    CallerProvider, CallerResult, DEFAULT_MAX_FRAMES, FrameContext, MemoryReader, RegisterFile,
    collect_backtrace,
};
use crate::{
    Backtrace, BreakpointLocation, Error, FrameKind, LoadedModule, ModuleImage, RegisterDescriptor,
    RegisterId, RegisterRole, RegisterSnapshot, RegisterValue, Result, StackFrame,
    ThreadId as DebugThreadId, UnwindTermination, VirtualAddress,
};

const CONTROLLER_THREAD_NAME: &str = "uscope-controller";
const WAITER_THREAD_NAME: &str = "uscope-waitpid";

fn backend_error(error: LinuxError) -> Error {
    Error::backend(error)
}

fn process_id(pid: Pid) -> ProcessId {
    ProcessId::new(u64::from(pid.as_raw().unsigned_abs()))
}

fn exception_info(signal: NixSignal) -> ExceptionInfo {
    ExceptionInfo::new(u64::from(signal as u32), signal.to_string())
}

pub type WaitEvent = WaitStatus;

struct Breakpoint {
    original_byte: u8,
}

#[derive(Clone)]
enum ExecutionState {
    Starting,
    Running,
    Stopped(StopReason),
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
}

struct Inferior {
    pid: Pid,
    loaded_module: LoadedModule,
    breakpoints: BTreeMap<VirtualAddress, Breakpoint>,
    stopped_at: Option<VirtualAddress>,
    state: ExecutionState,
    waiter: Option<JoinHandle<()>>,
}

enum WaitPhase {
    InitialExec,
    SingleStep(VirtualAddress),
    Continue,
}

struct PendingRun {
    reply: Reply<StopReason>,
    phase: WaitPhase,
}

struct Controller {
    executable: Arc<PathBuf>,
    module_image: Arc<ModuleImage>,
    unwind_info: Arc<dyn UnwindInfo>,
    messages: mpsc::Receiver<ControllerMessage>,
    message_sender: mpsc::Sender<ControllerMessage>,
    events: broadcast::Sender<DebuggerEvent>,
    ptrace: LinuxPtrace,
    inferior: Option<Inferior>,
    pending_breakpoints: Vec<BreakpointLocation>,
    pending_run: Option<PendingRun>,
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
    Ok(thread::Builder::new()
        .name(CONTROLLER_THREAD_NAME.into())
        .spawn(move || {
            Controller::new(
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
        executable: Arc<PathBuf>,
        module_image: Arc<ModuleImage>,
        unwind_info: Arc<dyn UnwindInfo>,
        messages: mpsc::Receiver<ControllerMessage>,
        message_sender: mpsc::Sender<ControllerMessage>,
        events: broadcast::Sender<DebuggerEvent>,
    ) -> Self {
        Self {
            executable,
            module_image,
            unwind_info,
            messages,
            message_sender,
            events,
            ptrace: LinuxPtrace::new(),
            inferior: None,
            pending_breakpoints: Vec::new(),
            pending_run: None,
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
            let Some(message) = self.messages.blocking_recv() else {
                break;
            };
            if let ControllerMessage::Wait(status) = message
                && !self.handle_wait(status)
            {
                break;
            }
        }
    }

    fn handle_request(&mut self, request: Request) -> bool {
        match request {
            Request::AddBreakpoint { location, reply } => {
                let result = self.add_breakpoint(location);
                let _ = reply.send(result);
            }
            Request::Launch { reply } => self.launch(reply),
            Request::Continue { reply } => self.resume(reply),
            Request::ReadWord { address, reply } => {
                let _ = reply.send(self.read_word(address));
            }
            Request::LoadedModule { reply } => {
                let _ = reply.send(self.loaded_module());
            }
            Request::StoppedLocation { reply } => {
                let _ = reply.send(self.stopped_location());
            }
            Request::Snapshot { reply } => {
                let _ = reply.send(Ok(self.snapshot()));
            }
            Request::Backtrace { reply } => {
                let _ = reply.send(self.backtrace());
            }
            Request::Registers { reply } => {
                let _ = reply.send(self.registers());
            }
            Request::Shutdown { reply } => {
                self.begin_shutdown(Some(reply));
                return self.inferior.is_some();
            }
        }

        true
    }

    fn handle_wait(&mut self, status: WaitStatus) -> bool {
        if self.shutdown_reply.is_some() {
            return self.handle_shutdown_wait(status);
        }

        let Some(pending) = self.pending_run.take() else {
            match status {
                WaitStatus::Exited(pid, code) => {
                    let _ = self.complete_exit(pid, ExitStatus::Code(i64::from(code)));
                }
                WaitStatus::Signaled(pid, signal, _) => {
                    let _ = self.complete_exit(pid, ExitStatus::Terminated(exception_info(signal)));
                }
                _ => self.fail_inferior(backend_error(LinuxError::UnexpectedWait(format!(
                    "{status:?}"
                )))),
            }
            return true;
        };

        match pending.phase {
            WaitPhase::InitialExec => self.handle_initial_stop(status, pending.reply),
            WaitPhase::SingleStep(address) => {
                self.handle_single_step(status, address, pending.reply);
            }
            WaitPhase::Continue => self.finish_run(status, pending.reply),
        }

        true
    }

    fn add_breakpoint(&mut self, location: BreakpointLocation) -> Result<()> {
        if self.pending_breakpoints.contains(&location) {
            return Ok(());
        }

        if let Some(inferior) = self.inferior.as_mut() {
            if !matches!(inferior.state, ExecutionState::Stopped(_)) {
                return Err(Error::NotStopped);
            }

            let address = match location {
                BreakpointLocation::Image(address) => {
                    inferior.loaded_module.virtual_address(address)?
                }
                BreakpointLocation::Virtual(address) => address,
            };

            self.ptrace.install_breakpoint(inferior, address)?;
        }
        self.pending_breakpoints.push(location);

        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::BreakpointsChanged {
            revision: self.revision,
        });

        Ok(())
    }

    fn launch(&mut self, reply: Reply<StopReason>) {
        if self.inferior.is_some() || self.pending_run.is_some() {
            let _ = reply.send(Err(Error::AlreadyRunning));
            return;
        }

        match self.ptrace.spawn(&self.executable) {
            Ok(pid) => {
                let waiter = match spawn_waiter(pid, self.message_sender.clone()) {
                    Ok(waiter) => waiter,
                    Err(error) => {
                        let _ = signal::kill(pid, NixSignal::SIGKILL);
                        let _ = waitpid(pid, Some(WaitPidFlag::__WALL));
                        let _ = reply.send(Err(error));
                        return;
                    }
                };
                let process_id = process_id(pid);

                self.inferior = Some(Inferior {
                    pid,
                    loaded_module: LoadedModule::main(self.module_image.id(), 0),
                    breakpoints: BTreeMap::new(),
                    stopped_at: None,
                    state: ExecutionState::Starting,
                    waiter: Some(waiter),
                });
                self.pending_run = Some(PendingRun {
                    reply,
                    phase: WaitPhase::InitialExec,
                });
                self.bump_revision();
                let _ = self
                    .events
                    .send(DebuggerEvent::InferiorLaunched { process_id });
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn resume(&mut self, reply: Reply<StopReason>) {
        if self.pending_run.is_some() {
            let _ = reply.send(Err(Error::AlreadyRunning));
            return;
        }

        let Some(inferior) = self.inferior.as_mut() else {
            let _ = reply.send(Err(Error::NotRunning));
            return;
        };

        if !matches!(inferior.state, ExecutionState::Stopped(_)) {
            let _ = reply.send(Err(Error::NotStopped));
            return;
        }

        let result = if let Some(address) = inferior.stopped_at.take() {
            self.ptrace
                .step(inferior.pid)
                .map(|()| WaitPhase::SingleStep(address))
        } else {
            self.ptrace
                .continue_execution(inferior.pid)
                .map(|()| WaitPhase::Continue)
        };

        match result {
            Ok(phase) => {
                inferior.state = ExecutionState::Running;
                self.pending_run = Some(PendingRun { reply, phase });
                self.bump_revision();
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn handle_initial_stop(&mut self, status: WaitStatus, reply: Reply<StopReason>) {
        let WaitStatus::Stopped(pid, NixSignal::SIGTRAP) = status else {
            let error = backend_error(LinuxError::UnexpectedWait(format!("{status:?}")));
            self.fail_run(reply, error);
            return;
        };

        match self.initialize_inferior(pid) {
            Ok(()) => {
                self.pending_run = Some(PendingRun {
                    reply,
                    phase: WaitPhase::Continue,
                });
                self.bump_revision();
            }
            Err(error) => self.fail_run(reply, error),
        }
    }

    fn initialize_inferior(&mut self, pid: Pid) -> Result<()> {
        self.ptrace.set_options(pid)?;
        let load_bias = load_bias(pid, &self.executable)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.loaded_module = LoadedModule::main(self.module_image.id(), load_bias);

        for &location in &self.pending_breakpoints {
            let address = match location {
                BreakpointLocation::Image(address) => {
                    inferior.loaded_module.virtual_address(address)?
                }
                BreakpointLocation::Virtual(address) => address,
            };
            self.ptrace.install_breakpoint(inferior, address)?;
        }

        self.ptrace.continue_execution(pid)?;
        inferior.state = ExecutionState::Running;
        Ok(())
    }

    fn handle_single_step(
        &mut self,
        status: WaitStatus,
        address: VirtualAddress,
        reply: Reply<StopReason>,
    ) {
        match status {
            WaitStatus::Stopped(pid, NixSignal::SIGTRAP) => {
                let result = self
                    .inferior
                    .as_ref()
                    .ok_or(Error::NotRunning)
                    .and_then(|inferior| self.ptrace.enable_breakpoint(inferior, address))
                    .and_then(|()| self.ptrace.continue_execution(pid));

                match result {
                    Ok(()) => {
                        self.pending_run = Some(PendingRun {
                            reply,
                            phase: WaitPhase::Continue,
                        });
                    }
                    Err(error) => self.fail_run(reply, error),
                }
            }
            other => {
                let result = self
                    .inferior
                    .as_ref()
                    .ok_or(Error::NotRunning)
                    .and_then(|inferior| self.ptrace.enable_breakpoint(inferior, address));

                match result {
                    Ok(()) => self.finish_run(other, reply),
                    Err(error) => self.fail_run(reply, error),
                }
            }
        }
    }

    fn finish_run(&mut self, status: WaitStatus, reply: Reply<StopReason>) {
        match self.complete_stop(status) {
            Ok(reason) => {
                let _ = reply.send(Ok(reason));
            }
            Err(error) => self.fail_run(reply, error),
        }
    }

    fn complete_stop(&mut self, status: WaitStatus) -> Result<StopReason> {
        let (pid, reason) = match status {
            WaitStatus::Stopped(pid, NixSignal::SIGTRAP) => {
                let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
                let mut registers = self.ptrace.registers(pid)?;
                let address = VirtualAddress::new(
                    registers.rip.checked_sub(1).ok_or(Error::AddressOverflow)?,
                );

                let reason = if inferior.breakpoints.contains_key(&address) {
                    self.ptrace.disable_breakpoint(inferior, address)?;
                    registers.rip = address.get();
                    self.ptrace.set_registers(pid, registers)?;
                    inferior.stopped_at = Some(address);
                    StopReason::Breakpoint { address }
                } else {
                    StopReason::Exception(exception_info(NixSignal::SIGTRAP))
                };

                (pid, reason)
            }
            WaitStatus::Stopped(pid, signal) => {
                (pid, StopReason::Exception(exception_info(signal)))
            }
            WaitStatus::Exited(pid, code) => {
                let status = ExitStatus::Code(i64::from(code));
                self.complete_exit(pid, status.clone())?;
                return Ok(StopReason::Exited(status));
            }
            WaitStatus::Signaled(pid, signal, _) => {
                let status = ExitStatus::Terminated(exception_info(signal));
                self.complete_exit(pid, status.clone())?;
                return Ok(StopReason::Exited(status));
            }
            other => {
                return Err(backend_error(LinuxError::UnexpectedWait(format!(
                    "{other:?}"
                ))));
            }
        };

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.state = ExecutionState::Stopped(reason.clone());
        self.bump_revision();
        let process_id = process_id(pid);
        let _ = self.events.send(DebuggerEvent::InferiorStopped {
            process_id,
            reason: reason.clone(),
        });
        Ok(reason)
    }

    fn complete_exit(&mut self, pid: Pid, status: ExitStatus) -> Result<()> {
        let mut inferior = self.inferior.take().ok_or(Error::NotRunning)?;
        if let Some(waiter) = inferior.waiter.take() {
            waiter.join().map_err(|_| Error::BackendThreadPanicked)?;
        }

        self.pending_run = None;
        self.bump_revision();
        let process_id = process_id(pid);
        let _ = self
            .events
            .send(DebuggerEvent::InferiorExited { process_id, status });
        Ok(())
    }

    fn read_word(&self, address: VirtualAddress) -> Result<u64> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        if !matches!(inferior.state, ExecutionState::Stopped(_)) {
            return Err(Error::NotStopped);
        }

        self.ptrace.read_word(inferior.pid, address.get())
    }

    fn stopped_location(&self) -> Result<(LoadedModule, VirtualAddress)> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let ExecutionState::Stopped(StopReason::Breakpoint { address }) = inferior.state else {
            return Err(if matches!(inferior.state, ExecutionState::Stopped(_)) {
                Error::LocationUnavailable
            } else {
                Error::NotStopped
            });
        };

        Ok((inferior.loaded_module, address))
    }

    fn loaded_module(&self) -> Result<LoadedModule> {
        self.inferior
            .as_ref()
            .map(|inferior| inferior.loaded_module)
            .ok_or(Error::NotRunning)
    }

    fn snapshot(&self) -> StateSnapshot {
        let inferior = self
            .inferior
            .as_ref()
            .map_or(InferiorState::NotRunning, |inferior| {
                let process_id = process_id(inferior.pid);
                match &inferior.state {
                    ExecutionState::Stopped(reason) => InferiorState::Stopped {
                        process_id,
                        reason: reason.clone(),
                    },
                    ExecutionState::Starting | ExecutionState::Running => {
                        InferiorState::Running { process_id }
                    }
                }
            });
        let breakpoints: Arc<[BreakpointLocation]> = self.pending_breakpoints.clone().into();

        StateSnapshot {
            revision: self.revision,
            inferior,
            breakpoints,
        }
    }

    fn backtrace(&self) -> Result<Backtrace> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        if !matches!(inferior.state, ExecutionState::Stopped(_)) {
            return Err(Error::NotStopped);
        }

        let native = self.ptrace.registers(inferior.pid)?;
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
                pid: inferior.pid,
            },
            first: true,
        };
        let module_image = Arc::clone(&self.module_image);
        let loaded_module = inferior.loaded_module;

        Ok(collect_backtrace(
            DebugThreadId::new(process_id(inferior.pid).get()),
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

    fn registers(&self) -> Result<RegisterSnapshot> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        if !matches!(inferior.state, ExecutionState::Stopped(_)) {
            return Err(Error::NotStopped);
        }

        let native = self.ptrace.registers(inferior.pid)?;

        Ok(x86_64_register_snapshot(
            self.revision,
            inferior.pid,
            self.module_image.target(),
            &native,
        ))
    }

    fn begin_shutdown(&mut self, reply: Option<Reply<()>>) {
        self.shutdown_reply = reply;
        self.pending_run = None;

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
                self.complete_exit(pid, ExitStatus::Code(i64::from(code)))
            }
            WaitStatus::Signaled(pid, signal, _) => {
                self.complete_exit(pid, ExitStatus::Terminated(exception_info(signal)))
            }
            WaitStatus::Stopped(_, _) => self.kill_inferior(),
            other => Err(backend_error(LinuxError::UnexpectedWait(format!(
                "{other:?}"
            )))),
        };

        if self.inferior.is_none() || result.is_err() {
            if let Some(reply) = self.shutdown_reply.take() {
                let _ = reply.send(result);
            }
            return false;
        }

        true
    }

    fn kill_inferior(&self) -> Result<()> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(());
        };
        match signal::kill(inferior.pid, NixSignal::SIGKILL) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
            Err(error) => Err(backend_error(LinuxError::System(error))),
        }
    }

    fn fail_inferior(&mut self, error: Error) {
        if let Some(pending) = self.pending_run.take() {
            let _ = pending.reply.send(Err(error));
        }
        let _ = self.kill_inferior();
    }

    fn fail_run(&self, reply: Reply<StopReason>, error: Error) {
        let _ = reply.send(Err(error));
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
        thread: DebugThreadId::new(process_id(pid).get()),
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
            .map_err(|error| backend_error(LinuxError::System(error)))?;
        Ok(())
    }

    fn continue_execution(&self, pid: Pid) -> Result<()> {
        self.assert_owner_thread();
        ptrace::cont(pid, None).map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn step(&self, pid: Pid) -> Result<()> {
        self.assert_owner_thread();
        ptrace::step(pid, None).map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn registers(&self, pid: Pid) -> Result<libc::user_regs_struct> {
        self.assert_owner_thread();
        ptrace::getregs(pid).map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn set_options(&self, pid: Pid) -> Result<()> {
        self.assert_owner_thread();
        ptrace::setoptions(pid, Options::PTRACE_O_EXITKILL)
            .map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn set_registers(&self, pid: Pid, registers: libc::user_regs_struct) -> Result<()> {
        self.assert_owner_thread();
        ptrace::setregs(pid, registers).map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn install_breakpoint(&self, inferior: &mut Inferior, address: VirtualAddress) -> Result<()> {
        self.assert_owner_thread();
        if inferior.breakpoints.contains_key(&address) {
            return Ok(());
        }

        let word = self.read_word(inferior.pid, address.get())?;
        let original_byte = word.to_ne_bytes()[0];
        self.write_word(inferior.pid, address.get(), (word & !0xff) | 0xcc)?;
        inferior
            .breakpoints
            .insert(address, Breakpoint { original_byte });
        Ok(())
    }

    fn disable_breakpoint(&self, inferior: &Inferior, address: VirtualAddress) -> Result<()> {
        self.assert_owner_thread();
        let breakpoint = inferior
            .breakpoints
            .get(&address)
            .expect("known breakpoint");
        let word = self.read_word(inferior.pid, address.get())?;
        self.write_word(
            inferior.pid,
            address.get(),
            (word & !0xff) | u64::from(breakpoint.original_byte),
        )
    }

    fn enable_breakpoint(&self, inferior: &Inferior, address: VirtualAddress) -> Result<()> {
        self.assert_owner_thread();
        assert!(
            inferior.breakpoints.contains_key(&address),
            "known breakpoint"
        );
        let word = self.read_word(inferior.pid, address.get())?;
        self.write_word(inferior.pid, address.get(), (word & !0xff) | 0xcc)
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

fn spawn_waiter(pid: Pid, messages: mpsc::Sender<ControllerMessage>) -> Result<JoinHandle<()>> {
    Ok(thread::Builder::new()
        .name(WAITER_THREAD_NAME.into())
        .spawn(move || {
            loop {
                let status = match waitpid(pid, Some(WaitPidFlag::__WALL)) {
                    Ok(status) => status,
                    Err(nix::errno::Errno::EINTR) => continue,
                    Err(_) => break,
                };
                let terminal = matches!(status, WaitStatus::Exited(..) | WaitStatus::Signaled(..));
                if messages
                    .blocking_send(ControllerMessage::Wait(status))
                    .is_err()
                    || terminal
                {
                    break;
                }
            }
        })?)
}

#[allow(
    unsafe_code,
    reason = "pre_exec is the only way to request PTRACE_TRACEME in the child"
)]
fn trace_child(command: &mut ProcessCommand) {
    // SAFETY: after fork, the closure only invokes the ptrace syscall and converts
    // errno without allocating or acquiring locks before Command performs exec.
    unsafe {
        command.pre_exec(|| {
            ptrace::traceme().map_err(|error| std::io::Error::from_raw_os_error(error as i32))
        });
    }
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
}
