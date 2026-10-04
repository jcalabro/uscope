//! The simulator's way into the Linux backend.
//!
//! [`SimTrace`] answers the controller's ptrace, waitpid, and `/proc`
//! requests from the simulated kernel. [`SimController`] is a real
//! [`Controller`] over it, whose queue the simulator's world serves one
//! message at a time, and which reports the ground truth oracles check.
//! Everything here translates between the backend's types and the
//! simulator's; the semantics live in `crate::sim::kernel`.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use nix::errno::Errno;
use nix::libc;
use nix::unistd::Pid;
use tokio::sync::{broadcast, mpsc};

use super::memory::MemoryAccessError;
use super::modules::{ModuleMapping, load_bias_in, parse_maps};
use super::native::{
    InspectionOps, LinuxTraceOps, maps_executable, siginfo_has_fault_address, siginfo_names_sender,
};
use super::registers::Fxsave;
use super::signals::{Signal, WaitEvent};
use super::{Controller, LinuxError, SessionLease, SignalMetadata, Waiter, backend_error};
use crate::backend::{ControllerChannels, ControllerMessage, ExecutableSource, FileIdentity};
use crate::debug_info::DebugInfo;
use crate::protocol::{DebuggerEvent, LaunchOptions, StopId};
use crate::sim::cpu::Registers;
use crate::sim::kernel::{Kernel, Options, SigInfo, Tid, WaitStatus};
use crate::sim::loader::Image;
use crate::{Error, Result, VirtualAddress};

/// The edge a simulated controller traces through, recorded in development
/// builds as production's is.
#[cfg(debug_assertions)]
type Edge = super::recorded::Recorded<SimTrace>;
#[cfg(not(debug_assertions))]
type Edge = SimTrace;

/// What a simulated launch starts.
pub struct SimLaunch {
    pub image: Arc<Image>,
    /// The path the executable has in the simulation.
    pub path: Arc<str>,
    /// The bytes `AT_RANDOM` names.
    pub random: [u8; 16],
}

/// The executable a simulated session debugs.
pub struct SimExecutable {
    pub path: Arc<str>,
    pub data: Arc<[u8]>,
    pub inode: u64,
}

/// Answers the controller's host requests from the simulated kernel.
pub struct SimTrace {
    kernel: Rc<RefCell<Kernel>>,
    launch: SimLaunch,
    /// Set once the controller starts its waiter.
    waiter: Rc<Cell<bool>>,
    /// Stop identifiers count per session, so that a session's identifiers
    /// do not depend on any other.
    last_stop_id: Cell<u64>,
}

fn system(errno: Errno) -> Error {
    backend_error(LinuxError::System(errno))
}

const fn tid(pid: Pid) -> Tid {
    pid.as_raw()
}

impl SimTrace {
    /// Records that the controller asked for something the simulation does
    /// not model, and fails the request.
    fn gap<T>(&self, request: &str) -> Result<T> {
        self.kernel
            .borrow_mut()
            .gap(format!("ptrace edge: {request}"));
        Err(system(Errno::ENOSYS))
    }

    fn maps(&self, pid: Pid) -> Result<String> {
        self.kernel
            .borrow()
            .maps(tid(pid))
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound).into())
    }
}

/// The view `PTRACE_GETSIGINFO` gives the controller, filtered as the
/// production edge filters a real `siginfo_t`.
#[cfg(test)]
#[must_use]
pub fn signal_view(info: SigInfo) -> (i32, i32, Option<i32>, Option<u64>) {
    let metadata = signal_metadata(info);
    (
        info.signal,
        metadata.code,
        metadata.sender,
        metadata.fault_address,
    )
}

fn signal_metadata(info: SigInfo) -> SignalMetadata {
    SignalMetadata {
        code: info.code,
        sender: siginfo_names_sender(info.code).then_some(info.pid),
        fault_address: siginfo_has_fault_address(info.signal, info.code).then_some(info.address),
    }
}

/// The kernel's registers as `PTRACE_GETREGS` reports them.
const fn user_registers(registers: &Registers) -> libc::user_regs_struct {
    let general = &registers.general;
    libc::user_regs_struct {
        r15: general[15],
        r14: general[14],
        r13: general[13],
        r12: general[12],
        rbp: general[5],
        rbx: general[3],
        r11: general[11],
        r10: general[10],
        r9: general[9],
        r8: general[8],
        rax: general[0],
        rcx: general[1],
        rdx: general[2],
        rsi: general[6],
        rdi: general[7],
        // Not inside a system call.
        orig_rax: u64::MAX,
        rip: registers.rip,
        cs: 0x33,
        eflags: registers.rflags,
        rsp: general[4],
        ss: 0x2b,
        fs_base: registers.fs_base,
        gs_base: registers.gs_base,
        ds: 0,
        es: 0,
        fs: 0,
        gs: 0,
    }
}

/// The flags `PTRACE_SETREGS` lets a tracer change; the rest keep their
/// values, as Linux's `set_flags` keeps them.
const SETTABLE_FLAGS: u64 = 0x0005_0dd5;

/// Applies `PTRACE_SETREGS` to the kernel's registers.
const fn apply_user_registers(registers: &mut Registers, user: &libc::user_regs_struct) {
    registers.general = [
        user.rax, user.rcx, user.rdx, user.rbx, user.rsp, user.rbp, user.rsi, user.rdi, user.r8,
        user.r9, user.r10, user.r11, user.r12, user.r13, user.r14, user.r15,
    ];
    registers.rip = user.rip;
    registers.rflags = (registers.rflags & !SETTABLE_FLAGS) | (user.eflags & SETTABLE_FLAGS);
    registers.fs_base = user.fs_base;
    registers.gs_base = user.gs_base;
}

fn wait_event(status: WaitStatus) -> WaitEvent {
    let pid = Pid::from_raw(status.tid());
    let signal = |number| Signal::new(number).expect("the kernel reports real signals");
    match status {
        WaitStatus::Exited(_, code) => WaitEvent::Exited(pid, code),
        WaitStatus::Signaled(_, number, core) => WaitEvent::Signaled(pid, signal(number), core),
        WaitStatus::Stopped(_, number) => WaitEvent::Stopped(pid, signal(number)),
        WaitStatus::Event(_, event) => WaitEvent::PtraceEvent(pid, Signal::SIGTRAP, event),
    }
}

fn wait_status(event: &WaitEvent) -> String {
    let status = match *event {
        WaitEvent::Exited(pid, code) => WaitStatus::Exited(pid.as_raw(), code),
        WaitEvent::Signaled(pid, signal, core) => {
            WaitStatus::Signaled(pid.as_raw(), signal.number(), core)
        }
        WaitEvent::Stopped(pid, signal) => WaitStatus::Stopped(pid.as_raw(), signal.number()),
        WaitEvent::PtraceEvent(pid, _, event) => WaitStatus::Event(pid.as_raw(), event),
        WaitEvent::PtraceSyscall(pid) | WaitEvent::Continued(pid) => {
            return format!("{event:?} for {pid}");
        }
    };
    status.to_string()
}

impl InspectionOps for SimTrace {
    fn read_word(&self, pid: Pid, address: u64) -> Result<u64> {
        self.kernel.borrow().peek(tid(pid), address).map_err(system)
    }

    fn read_memory_word(
        &self,
        pid: Pid,
        address: u64,
    ) -> std::result::Result<u64, MemoryAccessError> {
        match self.kernel.borrow().peek(tid(pid), address) {
            Ok(word) => Ok(word),
            Err(Errno::EIO) => Err(MemoryAccessError::Inaccessible),
            Err(errno) => Err(MemoryAccessError::Fatal(system(errno))),
        }
    }

    fn registers(&self, pid: Pid) -> Result<libc::user_regs_struct> {
        let registers = self
            .kernel
            .borrow()
            .get_registers(tid(pid))
            .map_err(system)?;
        Ok(user_registers(&registers))
    }

    fn floating_registers(&self, _pid: Pid) -> Result<Fxsave> {
        self.gap("floating-point registers")
    }

    fn tls_address(
        &self,
        _thread: Pid,
        _link_map: VirtualAddress,
        _offset: u64,
    ) -> std::result::Result<VirtualAddress, Arc<str>> {
        self.gap::<()>("thread-local storage")
            .map_err(|error| Arc::from(error.to_string()))?;
        unreachable!("a gap fails the request")
    }
}

impl LinuxTraceOps for SimTrace {
    fn spawn(&self, executable: &Path, options: LaunchOptions) -> Result<Pid> {
        if executable != Path::new(&*self.launch.path) {
            return self.gap(&format!("launching {}", executable.display()));
        }
        if !options.environment.is_empty()
            || options.working_directory.is_some()
            || options.stdin.is_some()
            || options.stdout.is_some()
            || options.stderr.is_some()
        {
            return self.gap("launch options other than arguments");
        }
        let Some(arguments) = options
            .arguments
            .iter()
            .map(|argument| argument.to_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
        else {
            return self.gap("arguments that are not UTF-8");
        };
        let tid = self.kernel.borrow_mut().spawn(
            Arc::clone(&self.launch.image),
            &self.launch.path,
            &arguments,
            self.launch.random,
        );
        Ok(Pid::from_raw(tid))
    }

    fn spawn_waiter(&self, _messages: mpsc::Sender<ControllerMessage>) -> Result<Waiter> {
        self.waiter.set(true);
        Ok(Waiter::external())
    }

    fn process_threads(&self, _process: Pid) -> Result<Vec<Pid>> {
        self.gap("listing threads")
    }

    fn traced_children(&self, _process: Pid, _thread: Pid) -> Vec<Pid> {
        // No simulated program forks: `fork` is a model gap.
        Vec::new()
    }

    fn thread_name(&self, process: Pid, thread: Pid) -> Option<Arc<str>> {
        let kernel = self.kernel.borrow();
        let owner = kernel.process_of(tid(thread))?;
        (owner.tgid == tid(process)).then(|| Arc::clone(&owner.name))
    }

    fn seize(&self, _pid: Pid, _exit_kill: bool) -> Result<bool> {
        self.gap("seize")
    }

    fn interrupt(&self, _pid: Pid) -> Result<bool> {
        self.gap("interrupt")
    }

    fn detach(&self, _pid: Pid, _signal: Option<Signal>) -> Result<bool> {
        self.gap("detach")
    }

    fn kill(&self, pid: Pid, signal: Signal) -> Result<()> {
        match self.kernel.borrow_mut().kill(tid(pid), signal.number()) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(errno) => Err(system(errno)),
        }
    }

    fn reap(&self, _pid: Pid) -> Result<()> {
        self.gap("reaping one thread")
    }

    fn wait_status(&self, _pid: Pid) -> std::result::Result<WaitEvent, Errno> {
        let _ = self.gap::<()>("waiting for one thread");
        Err(Errno::ENOSYS)
    }

    fn thread_group_id(&self, _pid: Pid) -> Result<Pid> {
        self.gap("thread group lookup")
    }

    fn process_start_time(&self, _process: Pid) -> Option<u64> {
        let _ = self.gap::<()>("process start time");
        None
    }

    fn tracer_process(&self) -> i32 {
        self.kernel.borrow().tracer()
    }

    fn allocate_stop_id(&self) -> StopId {
        let next = self.last_stop_id.get() + 1;
        self.last_stop_id.set(next);
        StopId::new(next)
    }

    fn identify_module(&self, _mapping: &ModuleMapping) -> Option<(PathBuf, u64)> {
        let _ = self.gap::<()>("identifying a library");
        None
    }

    fn load_module(&self, _path: &Path, _id: crate::ModuleImageId) -> Result<DebugInfo> {
        self.gap("loading a library")
    }

    fn load_bias(
        &self,
        pid: Pid,
        executable: &Path,
        executable_data: &[u8],
        identity: FileIdentity,
    ) -> Result<u64> {
        load_bias_in(&self.maps(pid)?, executable, executable_data, identity)
    }

    fn module_mappings(&self, pid: Pid) -> Result<Vec<ModuleMapping>> {
        let mut mappings = parse_maps(&self.maps(pid)?)?;
        mappings.retain(|mapping| mapping.executable);
        Ok(mappings)
    }

    fn write_word(&self, pid: Pid, address: u64, value: u64) -> Result<()> {
        self.kernel
            .borrow_mut()
            .poke(tid(pid), address, value)
            .map_err(system)
    }

    fn continue_execution(&self, pid: Pid, signal: Option<Signal>) -> Result<()> {
        self.kernel
            .borrow_mut()
            .resume(tid(pid), signal.map(Signal::number), false)
            .map_err(system)
    }

    fn continue_during_shutdown(&self, pid: Pid) -> Result<()> {
        match self
            .kernel
            .borrow_mut()
            .resume(tid(pid), Some(libc::SIGKILL), false)
        {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(errno) => Err(system(errno)),
        }
    }

    fn step(&self, pid: Pid, signal: Option<Signal>) -> Result<()> {
        self.kernel
            .borrow_mut()
            .resume(tid(pid), signal.map(Signal::number), true)
            .map_err(system)
    }

    fn set_registers(&self, pid: Pid, registers: libc::user_regs_struct) -> Result<()> {
        let mut kernel = self.kernel.borrow_mut();
        let mut current = kernel.get_registers(tid(pid)).map_err(system)?;
        apply_user_registers(&mut current, &registers);
        kernel.set_registers(tid(pid), current).map_err(system)
    }

    fn set_options(&self, pid: Pid, exit_kill: bool) -> Result<()> {
        self.kernel
            .borrow_mut()
            .set_options(
                tid(pid),
                Options {
                    trace_exit: true,
                    exit_kill,
                },
            )
            .map_err(system)
    }

    fn event_message(&self, pid: Pid) -> Result<libc::c_long> {
        self.kernel
            .borrow()
            .event_message(tid(pid))
            .map(u64::cast_signed)
            .map_err(system)
    }

    fn signal_metadata(&self, pid: Pid) -> std::result::Result<SignalMetadata, Errno> {
        self.kernel
            .borrow()
            .signal_info(tid(pid))
            .map(signal_metadata)
    }

    fn request_stop(&self, process: Pid, thread: Pid) -> Result<()> {
        self.kernel
            .borrow_mut()
            .tgkill(tid(process), tid(thread), libc::SIGSTOP)
            .map_err(system)
    }

    fn queued_trap(&self, pid: Pid) -> Result<bool> {
        Ok(self.kernel.borrow().trap_queued(tid(pid)))
    }

    fn executable(&self, pid: Pid, address: VirtualAddress) -> Result<bool> {
        Ok(maps_executable(&self.maps(pid)?, address))
    }

    fn read_debug_register(&self, _pid: Pid, _index: usize) -> std::result::Result<u64, Errno> {
        let _ = self.gap::<()>("reading a debug register");
        Err(Errno::ENOSYS)
    }

    fn write_debug_register(
        &self,
        _pid: Pid,
        _index: usize,
        _value: u64,
    ) -> std::result::Result<(), Errno> {
        let _ = self.gap::<()>("writing a debug register");
        Err(Errno::ENOSYS)
    }
}

/// A breakpoint site as the controller holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    pub original_byte: u8,
    pub installed: bool,
    /// How many owners the site has.
    pub owners: usize,
    /// The executions whose plans own the site.
    pub plans: Vec<u64>,
}

/// What the controller believes about the inferior, which oracles compare
/// with the simulated kernel.
#[derive(Debug, Default)]
pub struct Truth {
    /// Every breakpoint site of the inferior's address space.
    pub sites: BTreeMap<u64, Site>,
    /// The sites each execution plan installed, by execution.
    pub plan_sites: BTreeMap<u64, BTreeSet<u64>>,
    /// The execution in progress.
    pub active_execution: Option<u64>,
    /// The published stop, if the inferior is stopped.
    pub public_stop: Option<u64>,
    /// The process the controller debugs.
    pub inferior: Option<Tid>,
}

/// A real controller over the simulated kernel, with the queue it serves.
pub struct SimController {
    controller: Controller<Edge>,
    /// The controller's own sender, through which the simulated waiter
    /// queues statuses.
    waiter_messages: mpsc::Sender<ControllerMessage>,
}

/// The channels a simulated client talks to the controller through.
pub struct ClientChannels {
    pub requests: mpsc::Sender<ControllerMessage>,
    pub events: broadcast::Sender<DebuggerEvent>,
}

impl SimController {
    /// Builds a controller for `executable`, whose request queue holds
    /// `queue_capacity` messages and whose event channel holds
    /// `event_capacity` events.
    #[must_use]
    pub fn new(
        kernel: Rc<RefCell<Kernel>>,
        waiter: Rc<Cell<bool>>,
        launch: SimLaunch,
        executable: &SimExecutable,
        debug_info: DebugInfo,
        queue_capacity: usize,
        event_capacity: usize,
    ) -> (Self, ClientChannels) {
        let (sender, receiver) = mpsc::channel(queue_capacity);
        let (events, _) = broadcast::channel(event_capacity);
        let trace = SimTrace {
            kernel,
            launch,
            waiter,
            last_stop_id: Cell::new(0),
        };
        #[cfg(debug_assertions)]
        let trace = super::recorded::Recorded(trace);
        let controller = Controller::new(
            // A simulated session traces nothing real.
            SessionLease::detached(),
            ExecutableSource {
                display_path: Arc::new(PathBuf::from(&*executable.path)),
                data: Arc::clone(&executable.data),
                identity: FileIdentity {
                    inode: executable.inode,
                },
                process_start_time: None,
            },
            debug_info,
            ControllerChannels {
                sender: sender.clone(),
                receiver,
                events: events.clone().into(),
            },
            trace,
        );
        (
            Self {
                controller,
                waiter_messages: sender.clone(),
            },
            ClientChannels {
                requests: sender,
                events,
            },
        )
    }

    /// Whether a message waits in the queue.
    #[must_use]
    pub fn has_message(&self) -> bool {
        !self.controller.messages.is_empty()
    }

    /// Whether the queue has room for another message.
    #[must_use]
    pub fn has_room(&self) -> bool {
        self.waiter_messages.capacity() > 0
    }

    /// Serves the message at the front of the queue. Returns what it was
    /// and whether the controller keeps running, or `None` when the queue
    /// is empty.
    pub fn deliver(&mut self) -> Option<(String, bool)> {
        let message = self.controller.messages.try_recv().ok()?;
        let description = match &message {
            ControllerMessage::Request(request) => request.describe(),
            ControllerMessage::Wait(event) => format!("wait {}", wait_status(event)),
        };
        Some((description, self.controller.handle_message(message)))
    }

    /// Queues a status the simulated waiter reaped. Returns whether the
    /// queue had room.
    pub fn queue_status(&self, status: WaitStatus) -> bool {
        self.waiter_messages
            .try_send(ControllerMessage::Wait(wait_event(status)))
            .is_ok()
    }

    /// The controller's beliefs about the inferior.
    #[must_use]
    pub fn truth(&self) -> Truth {
        let Some(inferior) = self.controller.inferior.as_ref() else {
            return Truth::default();
        };
        Truth {
            sites: inferior
                .breakpoints
                .iter()
                .map(|(address, site)| {
                    (
                        address.get(),
                        Site {
                            original_byte: site.original_byte,
                            installed: site.installed,
                            owners: site.owners.len(),
                            plans: site
                                .owners
                                .iter()
                                .filter_map(|owner| match owner {
                                    super::BreakpointOwner::Plan(execution) => {
                                        Some(execution.get())
                                    }
                                    _ => None,
                                })
                                .collect(),
                        },
                    )
                })
                .collect(),
            plan_sites: inferior
                .plan_sites
                .iter()
                .map(|(execution, sites)| {
                    (
                        execution.get(),
                        sites.iter().map(|address| address.get()).collect(),
                    )
                })
                .collect(),
            active_execution: inferior.active.as_ref().map(|active| active.id.get()),
            public_stop: inferior.public_stop.as_ref().map(|stop| stop.id.get()),
            inferior: Some(inferior.tgid.as_raw()),
        }
    }
}

/// A real traced process, for tests that compare the simulation with
/// Linux. It is killed and reaped when dropped.
#[cfg(test)]
pub struct NativeTracee {
    ptrace: super::native::LinuxPtrace,
    pid: Pid,
}

#[cfg(test)]
impl NativeTracee {
    /// Launches `executable` traced, as the controller does, and returns
    /// once it reported its first stop, which is returned too.
    #[must_use]
    pub fn spawn(executable: &Path, arguments: &[String]) -> (Self, WaitStatus) {
        let ptrace = super::native::LinuxPtrace::new();
        let options = LaunchOptions {
            arguments: arguments.iter().map(Into::into).collect(),
            stdout: Some(std::process::Stdio::null()),
            ..LaunchOptions::default()
        };
        let pid = ptrace
            .spawn(executable, options)
            .expect("spawn a traced program");
        let tracee = Self { ptrace, pid };
        let first = tracee.wait();
        (tracee, first)
    }

    #[must_use]
    pub const fn pid(&self) -> Tid {
        self.pid.as_raw()
    }

    /// Waits for the tracee's next status, failing after ten seconds.
    #[must_use]
    pub fn wait(&self) -> WaitStatus {
        let started = std::time::Instant::now();
        let event = loop {
            match super::native::wait_for(self.pid, libc::__WALL | libc::WNOHANG) {
                Ok(Some(event)) => break event,
                Ok(None) | Err(Errno::EINTR) => {
                    let waited = started.elapsed();
                    assert!(
                        waited < std::time::Duration::from_secs(10),
                        "{} reported nothing for ten seconds",
                        self.pid
                    );
                    // A single step reports within microseconds.
                    if waited < std::time::Duration::from_millis(1) {
                        std::thread::yield_now();
                    } else {
                        std::thread::sleep(std::time::Duration::from_micros(100));
                    }
                }
                Err(errno) => panic!("waiting for {} failed: {errno}", self.pid),
            }
        };
        match event {
            WaitEvent::Exited(pid, code) => WaitStatus::Exited(pid.as_raw(), code),
            WaitEvent::Signaled(pid, signal, core) => {
                WaitStatus::Signaled(pid.as_raw(), signal.number(), core)
            }
            WaitEvent::Stopped(pid, signal) => WaitStatus::Stopped(pid.as_raw(), signal.number()),
            WaitEvent::PtraceEvent(pid, _, event) => WaitStatus::Event(pid.as_raw(), event),
            other => panic!("unexpected status {other:?}"),
        }
    }

    pub fn registers(&self) -> std::result::Result<Registers, Errno> {
        let user = self.ptrace.registers(self.pid).map_err(errno_of)?;
        let mut registers = Registers::default();
        apply_user_registers(&mut registers, &user);
        registers.rflags = user.eflags;
        Ok(registers)
    }

    pub fn set_registers(&self, registers: &Registers) -> std::result::Result<(), Errno> {
        self.ptrace
            .set_registers(self.pid, user_registers(registers))
            .map_err(errno_of)
    }

    /// `PTRACE_GETSIGINFO`, as the controller sees it.
    pub fn signal_view(&self) -> std::result::Result<(i32, i32, Option<i32>, Option<u64>), Errno> {
        let raw = nix::sys::ptrace::getsiginfo(self.pid)?;
        let metadata = super::native::signal_metadata(&raw);
        Ok((
            raw.si_signo,
            metadata.code,
            metadata.sender,
            metadata.fault_address,
        ))
    }

    pub fn event_message(&self) -> std::result::Result<u64, Errno> {
        self.ptrace
            .event_message(self.pid)
            .map(i64::cast_unsigned)
            .map_err(errno_of)
    }

    pub fn set_options(&self, exit_kill: bool) -> std::result::Result<(), Errno> {
        self.ptrace
            .set_options(self.pid, exit_kill)
            .map_err(errno_of)
    }

    pub fn resume(&self, signal: Option<i32>, single_step: bool) -> std::result::Result<(), Errno> {
        let signal = signal.map(|number| Signal::new(number).expect("a real signal"));
        if single_step {
            self.ptrace.step(self.pid, signal)
        } else {
            self.ptrace.continue_execution(self.pid, signal)
        }
        .map_err(errno_of)
    }

    pub fn kill(&self, signal: i32) -> std::result::Result<(), Errno> {
        nix::sys::signal::kill(self.pid, nix::sys::signal::Signal::try_from(signal)?)
    }

    pub fn request_stop(&self) -> std::result::Result<(), Errno> {
        self.ptrace
            .request_stop(self.pid, self.pid)
            .map_err(errno_of)
    }

    pub fn peek(&self, address: u64) -> std::result::Result<u64, Errno> {
        nix::sys::ptrace::read(self.pid, address as nix::sys::ptrace::AddressType)
            .map(i64::cast_unsigned)
    }

    pub fn poke(&self, address: u64, value: u64) -> std::result::Result<(), Errno> {
        self.ptrace
            .write_word(self.pid, address, value)
            .map_err(errno_of)
    }

    /// `/proc/<pid>/maps`.
    #[must_use]
    pub fn maps(&self) -> String {
        std::fs::read_to_string(format!("/proc/{}/maps", self.pid)).expect("read the maps")
    }

    /// Reads memory through `/proc/<pid>/mem`, or `None` where the kernel
    /// refuses.
    #[must_use]
    pub fn read_memory(&self, address: u64, length: usize) -> Option<Vec<u8>> {
        use std::os::unix::fs::FileExt as _;
        let file = std::fs::File::open(format!("/proc/{}/mem", self.pid)).ok()?;
        let mut bytes = vec![0; length];
        file.read_exact_at(&mut bytes, address).ok()?;
        Some(bytes)
    }
}

#[cfg(test)]
fn errno_of(error: Error) -> Errno {
    match error {
        Error::Backend(error) => match error.downcast_ref::<LinuxError>() {
            Some(LinuxError::System(errno)) => *errno,
            _ => panic!("not a system error: {error}"),
        },
        other => panic!("not a system error: {other}"),
    }
}

#[cfg(test)]
impl Drop for NativeTracee {
    fn drop(&mut self) {
        let _ = nix::sys::signal::kill(self.pid, nix::sys::signal::Signal::SIGKILL);
        // Reap every remaining status, so nothing outlives the test. A
        // thread in a stop, even one already reported such as its exit
        // event, waits there until it is continued.
        loop {
            let _ = self.ptrace.continue_execution(self.pid, None);
            match super::native::wait_for(self.pid, libc::__WALL) {
                Ok(Some(WaitEvent::Exited(..) | WaitEvent::Signaled(..)) | None) | Err(_) => {
                    break;
                }
                Ok(Some(_)) => {}
            }
        }
    }
}
