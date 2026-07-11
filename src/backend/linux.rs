use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::rc::Rc;
use std::sync::Arc;
use std::thread::{self, JoinHandle, ThreadId};

use nix::libc;
use nix::sys::ptrace;
use nix::sys::signal::{self, Signal as NixSignal};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use tokio::sync::{broadcast, mpsc};

use super::ControllerMessage;
use crate::protocol::{
    DebuggerEvent, ExceptionInfo, ExitStatus, InferiorState, ProcessId, Reply, Request,
    StateSnapshot, StopReason,
};
use crate::{BreakpointLocation, Error, LoadedModule, ModuleImageId, Result, VirtualAddress};

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
    module_image: ModuleImageId,
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
    module_image: ModuleImageId,
    message_sender: mpsc::Sender<ControllerMessage>,
    messages: mpsc::Receiver<ControllerMessage>,
    events: broadcast::Sender<DebuggerEvent>,
) -> Result<JoinHandle<()>> {
    Ok(thread::Builder::new()
        .name(CONTROLLER_THREAD_NAME.into())
        .spawn(move || {
            Controller::new(executable, module_image, messages, message_sender, events).run();
        })?)
}

impl Controller {
    fn new(
        executable: Arc<PathBuf>,
        module_image: ModuleImageId,
        messages: mpsc::Receiver<ControllerMessage>,
        message_sender: mpsc::Sender<ControllerMessage>,
        events: broadcast::Sender<DebuggerEvent>,
    ) -> Self {
        Self {
            executable,
            module_image,
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
                    loaded_module: LoadedModule::main(self.module_image, 0),
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
        let load_bias = load_bias(pid, &self.executable)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.loaded_module = LoadedModule::main(self.module_image, load_bias);

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
            other => self.finish_run(other, reply),
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
        return u64::from_str_radix(start, 16)
            .map_err(|_| backend_error(LinuxError::LoadBias(executable.as_ref().into())));
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
