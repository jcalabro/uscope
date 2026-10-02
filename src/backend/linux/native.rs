//! The ptrace, waitpid, and /proc edge every controller operation goes through.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::marker::PhantomData;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command as ProcessCommand;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, Thread, ThreadId};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::libc;
use nix::sys::ptrace::{self, Options};
use nix::sys::signal::{self, Signal as NixSignal};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use tokio::sync::mpsc;

use crate::backend::linux::thread_db;
use crate::backend::{ControllerMessage, FileIdentity};
use crate::{Error, Result, VirtualAddress};

use super::memory::MemoryAccessError;
use super::modules::{ModuleMapping, load_bias, module_mappings};
use super::registers::{Fxsave, native_fxsave};
use super::{
    BREAKPOINT_OPCODE, BreakpointOwner, BreakpointSite, LinuxError, SignalMetadata,
    WAITER_THREAD_NAME, Waiter, backend_error,
};

/// Read-only access to a stopped target's registers, memory, and thread-local
/// storage. Live tracing and post-mortem targets share every inspection path.
pub(super) trait InspectionOps {
    fn read_word(&self, pid: Pid, address: u64) -> Result<u64>;
    fn read_memory_word(
        &self,
        pid: Pid,
        address: u64,
    ) -> std::result::Result<u64, MemoryAccessError> {
        self.read_word(pid, address)
            .map_err(MemoryAccessError::Fatal)
    }
    fn registers(&self, pid: Pid) -> Result<libc::user_regs_struct>;
    fn floating_registers(&self, _pid: Pid) -> Result<Fxsave> {
        Err(backend_error(LinuxError::UnsupportedFloatingRegisters))
    }
    /// Resolves a module's thread-local block for one thread.
    fn tls_address(
        &self,
        _thread: Pid,
        _link_map: VirtualAddress,
        _offset: u64,
    ) -> std::result::Result<VirtualAddress, Arc<str>> {
        Err("thread-local storage lookup is unsupported by this target".into())
    }
}

pub(super) trait LinuxTraceOps: InspectionOps {
    fn spawn(&self, executable: &Path) -> Result<Pid>;
    fn spawn_waiter(&self, messages: mpsc::Sender<ControllerMessage>) -> Result<Waiter>;
    fn process_threads(&self, process: Pid) -> Result<Vec<Pid>>;
    fn seize(&self, pid: Pid) -> Result<bool>;
    fn interrupt(&self, pid: Pid) -> Result<bool>;
    fn detach(&self, pid: Pid, signal: Option<NixSignal>) -> Result<()>;
    fn kill(&self, pid: Pid, signal: NixSignal) -> Result<()>;
    fn reap(&self, pid: Pid) -> Result<()>;
    fn thread_group_id(&self, pid: Pid) -> Result<Pid>;
    fn load_bias(
        &self,
        pid: Pid,
        executable: &Path,
        executable_data: &[u8],
        identity: FileIdentity,
    ) -> Result<u64>;
    fn module_mappings(&self, _pid: Pid) -> Result<Vec<ModuleMapping>> {
        // Deterministic effect fakes opt out of host /proc inspection. The
        // production ptrace edge overrides this method.
        Ok(Vec::new())
    }
    fn write_word(&self, pid: Pid, address: u64, value: u64) -> Result<()>;
    fn continue_execution(&self, pid: Pid, signal: Option<NixSignal>) -> Result<()>;
    fn continue_during_shutdown(&self, pid: Pid) -> Result<()>;
    fn step(&self, pid: Pid, signal: Option<NixSignal>) -> Result<()>;
    fn set_registers(&self, pid: Pid, registers: libc::user_regs_struct) -> Result<()>;
    fn set_options(&self, pid: Pid, exit_kill: bool) -> Result<()>;
    fn event_message(&self, pid: Pid) -> Result<libc::c_long>;
    fn signal_metadata(&self, pid: Pid) -> std::result::Result<SignalMetadata, Errno>;
    fn request_stop(&self, process: Pid, thread: Pid) -> Result<()>;
    /// Whether a stopped thread's private pending set holds a deliverable
    /// SIGTRAP that has not been reported yet.
    fn queued_trap(&self, _pid: Pid) -> Result<bool> {
        // Deterministic effect fakes report nothing queued. The production
        // ptrace edge overrides this method.
        Ok(false)
    }
    /// Reads one x86-64 debug register from a stopped thread's user area.
    fn read_debug_register(&self, pid: Pid, index: usize) -> std::result::Result<u64, Errno>;
    /// Writes one x86-64 debug register in a stopped thread's user area.
    fn write_debug_register(
        &self,
        pid: Pid,
        index: usize,
        value: u64,
    ) -> std::result::Result<(), Errno>;
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

pub(super) struct LinuxPtrace {
    pub(super) affinity: ThreadAffinity,
    pub(super) not_send_or_sync: PhantomData<Rc<()>>,
    /// The current inferior's waiter, woken by every request that makes a
    /// tracee report a new wait status.
    waiter: RefCell<Option<Thread>>,
}

impl LinuxPtrace {
    pub(super) fn new() -> Self {
        Self {
            affinity: ThreadAffinity::new(),
            not_send_or_sync: PhantomData,
            waiter: RefCell::new(None),
        }
    }

    pub(super) fn assert_owner_thread(&self) {
        self.affinity.assert_owner();
    }

    /// Makes the waiter poll promptly for the status a request just caused,
    /// instead of after its idle backoff. Requests wake it only after the
    /// syscall, so the poll the wake triggers cannot run too early to see
    /// the status and leave the waiter backing off.
    fn wake_waiter(&self) {
        if let Some(waiter) = self.waiter.borrow().as_ref() {
            waiter.unpark();
        }
    }
}

impl InspectionOps for LinuxPtrace {
    fn read_word(&self, pid: Pid, address: u64) -> Result<u64> {
        self.assert_owner_thread();
        let value = ptrace::read(pid, address as ptrace::AddressType)
            .map_err(|error| backend_error(LinuxError::System(error)))?;
        Ok(u64::from_ne_bytes(value.to_ne_bytes()))
    }

    fn read_memory_word(
        &self,
        pid: Pid,
        address: u64,
    ) -> std::result::Result<u64, MemoryAccessError> {
        self.assert_owner_thread();
        match ptrace::read(pid, address as ptrace::AddressType) {
            Ok(value) => Ok(u64::from_ne_bytes(value.to_ne_bytes())),
            Err(Errno::EFAULT | Errno::EIO) => Err(MemoryAccessError::Inaccessible),
            Err(error) => Err(MemoryAccessError::Fatal(backend_error(LinuxError::System(
                error,
            )))),
        }
    }

    fn registers(&self, pid: Pid) -> Result<libc::user_regs_struct> {
        self.assert_owner_thread();
        ptrace::getregs(pid).map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn floating_registers(&self, pid: Pid) -> Result<Fxsave> {
        self.assert_owner_thread();
        ptrace::getregset::<ptrace::regset::NT_PRFPREG>(pid)
            .map(|registers| native_fxsave(&registers))
            .map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn tls_address(
        &self,
        thread: Pid,
        link_map: VirtualAddress,
        offset: u64,
    ) -> std::result::Result<VirtualAddress, Arc<str>> {
        self.assert_owner_thread();
        let process = thread_group_id(thread).map_err(|error| Arc::from(error.to_string()))?;
        thread_db::tls_address(
            &thread_db::LiveProcess { pid: process },
            process,
            thread,
            link_map,
            offset,
        )
    }
}

impl LinuxTraceOps for LinuxPtrace {
    fn spawn_waiter(&self, messages: mpsc::Sender<ControllerMessage>) -> Result<Waiter> {
        self.assert_owner_thread();
        let waiter = spawn_waiter(messages)?;
        *self.waiter.borrow_mut() = Some(waiter.thread.thread().clone());
        Ok(waiter)
    }

    fn process_threads(&self, process: Pid) -> Result<Vec<Pid>> {
        self.assert_owner_thread();
        process_threads(process)
    }

    fn seize(&self, pid: Pid) -> Result<bool> {
        self.assert_owner_thread();
        match ptrace::seize(pid, trace_options(false)) {
            Ok(()) => Ok(true),
            Err(Errno::ESRCH) => Ok(false),
            Err(error) => Err(backend_error(LinuxError::System(error))),
        }
    }

    fn interrupt(&self, pid: Pid) -> Result<bool> {
        self.assert_owner_thread();
        match ptrace::interrupt(pid) {
            Ok(()) => {
                self.wake_waiter();
                Ok(true)
            }
            Err(Errno::ESRCH) => Ok(false),
            Err(error) => Err(backend_error(LinuxError::System(error))),
        }
    }

    fn detach(&self, pid: Pid, signal: Option<NixSignal>) -> Result<()> {
        self.assert_owner_thread();
        match ptrace::detach(pid, signal) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(error) => Err(backend_error(LinuxError::System(error))),
        }
    }

    fn kill(&self, pid: Pid, signal: NixSignal) -> Result<()> {
        self.assert_owner_thread();
        let result = signal::kill(pid, signal);
        self.wake_waiter();
        match result {
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

    fn load_bias(
        &self,
        pid: Pid,
        executable: &Path,
        executable_data: &[u8],
        identity: FileIdentity,
    ) -> Result<u64> {
        self.assert_owner_thread();
        load_bias(pid, executable, executable_data, identity)
    }

    fn module_mappings(&self, pid: Pid) -> Result<Vec<ModuleMapping>> {
        self.assert_owner_thread();
        module_mappings(pid)
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

    fn write_word(&self, pid: Pid, address: u64, value: u64) -> Result<()> {
        self.assert_owner_thread();
        let value = libc::c_long::from_ne_bytes(value.to_ne_bytes());
        ptrace::write(pid, address as ptrace::AddressType, value)
            .map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn continue_execution(&self, pid: Pid, signal: Option<NixSignal>) -> Result<()> {
        self.assert_owner_thread();
        ptrace::cont(pid, signal).map_err(|error| backend_error(LinuxError::System(error)))?;
        self.wake_waiter();
        Ok(())
    }

    fn continue_during_shutdown(&self, pid: Pid) -> Result<()> {
        self.assert_owner_thread();
        let result = ptrace::cont(pid, Some(NixSignal::SIGKILL));
        self.wake_waiter();
        match result {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(error) => Err(backend_error(LinuxError::System(error))),
        }
    }

    fn step(&self, pid: Pid, signal: Option<NixSignal>) -> Result<()> {
        self.assert_owner_thread();
        ptrace::step(pid, signal).map_err(|error| backend_error(LinuxError::System(error)))?;
        self.wake_waiter();
        Ok(())
    }

    fn set_registers(&self, pid: Pid, registers: libc::user_regs_struct) -> Result<()> {
        self.assert_owner_thread();
        ptrace::setregs(pid, registers).map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn set_options(&self, pid: Pid, exit_kill: bool) -> Result<()> {
        self.assert_owner_thread();
        ptrace::setoptions(pid, trace_options(exit_kill))
            .map_err(|error| backend_error(LinuxError::System(error)))
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
        tgkill(process, thread, NixSignal::SIGSTOP)?;
        self.wake_waiter();
        Ok(())
    }

    fn queued_trap(&self, pid: Pid) -> Result<bool> {
        self.assert_owner_thread();
        let status = match fs::read_to_string(format!("/proc/{pid}/status")) {
            Ok(status) => status,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        Ok(queued_trap_in_status(&status))
    }

    fn read_debug_register(&self, pid: Pid, index: usize) -> std::result::Result<u64, Errno> {
        self.assert_owner_thread();
        ptrace::read_user(pid, debug_register_offset(index))
            .map(|value| u64::from_ne_bytes(value.to_ne_bytes()))
    }

    fn write_debug_register(
        &self,
        pid: Pid,
        index: usize,
        value: u64,
    ) -> std::result::Result<(), Errno> {
        self.assert_owner_thread();
        ptrace::write_user(
            pid,
            debug_register_offset(index),
            libc::c_long::from_ne_bytes(value.to_ne_bytes()),
        )
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

/// Whether a ptrace request failed because the tracee left its ptrace-stop.
pub(super) fn is_vanished_tracee(error: &Error) -> bool {
    matches!(
        error,
        Error::Backend(error)
            if matches!(error.downcast_ref::<LinuxError>(), Some(LinuxError::System(Errno::ESRCH)))
    )
}

/// Whether `/proc/<tid>/status` shows SIGTRAP pending for the thread itself
/// and not blocked. A malformed mask is treated as nothing queued.
pub(super) fn queued_trap_in_status(status: &str) -> bool {
    let mask = |field: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(field))
            .and_then(|value| u64::from_str_radix(value.trim(), 16).ok())
            .unwrap_or(0)
    };
    let trap = 1 << (NixSignal::SIGTRAP as u32 - 1);
    mask("SigPnd:") & trap != 0 && mask("SigBlk:") & trap == 0
}

/// The `struct user` offset of one debug register, as `PTRACE_PEEKUSER` and
/// `PTRACE_POKEUSER` address it.
pub(super) fn debug_register_offset(index: usize) -> ptrace::AddressType {
    assert!(index < 8, "x86-64 has eight debug registers");
    (std::mem::offset_of!(libc::user, u_debugreg) + index * std::mem::size_of::<u64>())
        as ptrace::AddressType
}

#[derive(Clone, Copy)]
pub(super) struct ThreadAffinity {
    pub(super) owner: ThreadId,
}

impl ThreadAffinity {
    pub(super) fn new() -> Self {
        Self {
            owner: thread::current().id(),
        }
    }

    pub(super) fn assert_owner(self) {
        assert_eq!(
            self.owner,
            thread::current().id(),
            "ptrace called from non-controller thread"
        );
    }
}

/// The first poll interval after a wake or a status. Single steps and
/// breakpoint repairs complete within microseconds, so their stops are
/// collected almost at once.
const WAITER_MIN_POLL: Duration = Duration::from_micros(20);
/// The longest poll interval while the inferior runs without reporting.
const WAITER_MAX_POLL: Duration = Duration::from_millis(5);

/// Spawns the thread that collects every tracee's wait statuses.
///
/// It polls without blocking so it can be stopped while other children of
/// the debugger's process keep running. The interval doubles while nothing
/// happens and drops to the minimum after each status and each wake from
/// the controller, which wakes it whenever a request will cause a status.
pub(super) fn spawn_waiter(messages: mpsc::Sender<ControllerMessage>) -> Result<Waiter> {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread = thread::Builder::new()
        .name(WAITER_THREAD_NAME.into())
        .spawn(move || {
            let mut interval = WAITER_MIN_POLL;
            while !thread_stop.load(Ordering::Acquire) {
                let status = match waitpid(
                    Pid::from_raw(-1),
                    Some(WaitPidFlag::__WALL | WaitPidFlag::WNOHANG),
                ) {
                    Ok(status) => status,
                    Err(Errno::EINTR) => continue,
                    Err(_) => break,
                };
                if status == WaitStatus::StillAlive {
                    let parked = Instant::now();
                    thread::park_timeout(interval);
                    // Returning early means the controller woke the waiter.
                    interval = if parked.elapsed() < interval {
                        WAITER_MIN_POLL
                    } else {
                        interval.saturating_mul(2).min(WAITER_MAX_POLL)
                    };
                    continue;
                }
                interval = WAITER_MIN_POLL;
                if messages
                    .blocking_send(ControllerMessage::Wait(status))
                    .is_err()
                {
                    break;
                }
            }
        })?;
    Ok(Waiter { stop, thread })
}

#[allow(
    unsafe_code,
    reason = "pre_exec is the only way to establish child-side ptrace and parent-death behavior"
)]
pub(super) fn trace_child(command: &mut ProcessCommand) {
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
pub(super) fn tgkill(process: Pid, thread: Pid, signal: NixSignal) -> Result<()> {
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
pub(super) fn signal_metadata(info: &libc::siginfo_t) -> SignalMetadata {
    let code = info.si_code;
    let sender = if siginfo_names_sender(code) {
        // SAFETY: these si_code values select a siginfo layout containing si_pid.
        Some(unsafe { info.si_pid() })
    } else {
        None
    };
    SignalMetadata { code, sender }
}

/// Whether a signal's siginfo names the process that sent it. Other user
/// codes reuse that field: `SI_TIMER` for a timer ID and `SI_SIGIO` for a
/// poll band.
pub(super) const fn siginfo_names_sender(code: i32) -> bool {
    code == libc::SI_USER || (code < 0 && code != libc::SI_TIMER && code != libc::SI_SIGIO)
}

/// Whether a signal's siginfo holds a faulting address: a synchronous fault
/// raised by the kernel for an instruction. `SI_KERNEL` faults, such as a
/// general-protection fault on a non-canonical address, record none.
pub(super) const fn siginfo_has_fault_address(number: i32, code: i32) -> bool {
    matches!(
        number,
        libc::SIGSEGV | libc::SIGBUS | libc::SIGILL | libc::SIGFPE | libc::SIGTRAP
    ) && code > 0
        && code != libc::SI_KERNEL
}

pub(super) const fn wait_status_pid(status: &WaitStatus) -> Option<Pid> {
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

pub(super) fn thread_group_id(pid: Pid) -> Result<Pid> {
    let path = format!("/proc/{pid}/status");
    let status = fs::read_to_string(&path)?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Tgid:"))
        .and_then(|value| value.trim().parse::<i32>().ok())
        .map(Pid::from_raw)
        .ok_or_else(|| backend_error(LinuxError::ProcFile(path)))
}

pub(super) fn process_threads(process: Pid) -> Result<Vec<Pid>> {
    let mut threads = fs::read_dir(format!("/proc/{process}/task"))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let raw = entry.file_name().to_str()?.parse::<i32>().ok()?;
            (raw > 0).then_some(Pid::from_raw(raw))
        })
        .collect::<Vec<_>>();
    threads.sort_unstable();
    Ok(threads)
}

pub(super) fn trace_options(exit_kill: bool) -> Options {
    // Fork children are traced only long enough to remove inherited traps.
    let common = Options::PTRACE_O_TRACECLONE
        | Options::PTRACE_O_TRACEFORK
        | Options::PTRACE_O_TRACEEXEC
        | Options::PTRACE_O_TRACEEXIT
        | Options::PTRACE_O_TRACESYSGOOD;
    if exit_kill {
        common | Options::PTRACE_O_EXITKILL
    } else {
        common
    }
}
