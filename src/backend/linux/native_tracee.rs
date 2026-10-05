//! A real traced process, which the simulator's conformance tests drive
//! beside the simulated kernel.

use std::path::Path;

use nix::errno::Errno;
use nix::libc;
use nix::unistd::Pid;

use super::LinuxError;
use super::native::{InspectionOps as _, LinuxTraceOps as _};
use super::signals::{Signal, WaitEvent};
use super::sim_edge::{SETTABLE_FLAGS, apply_user_registers, user_registers};
use crate::Error;
use crate::protocol::LaunchOptions;
use crate::sim::cpu::Registers;
use crate::sim::kernel::{Tid, WaitStatus};

/// A real traced process, for tests that compare the simulation with
/// Linux. Requests name the thread they address. Every thread is killed and
/// reaped when the tracee is dropped.
pub struct NativeTracee {
    ptrace: super::native::LinuxPtrace,
    pid: Pid,
    /// Perf breakpoints holding some of a thread's hardware breakpoints.
    held: Vec<std::os::fd::OwnedFd>,
    /// Children the process forked, killed with it.
    forked: Vec<Pid>,
    /// Whether the process was left to its parent rather than killed.
    abandoned: bool,
}

/// The fields of `struct perf_event_attr` up to its second version, which
/// a hardware breakpoint needs.
#[repr(C)]
struct PerfEventAttr {
    kind: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    breakpoint_type: u32,
    breakpoint_address: u64,
    breakpoint_length: u64,
}

#[expect(
    clippy::unused_self,
    reason = "requests about a thread go through the tracee it belongs to"
)]
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
        let tracee = Self {
            ptrace,
            pid,
            held: Vec::new(),
            forked: Vec::new(),
            abandoned: false,
        };
        let first = tracee.wait(tracee.pid());
        (tracee, first)
    }

    /// Launches `executable` traced as [`Self::spawn`] does, except that it
    /// outlives the thread that traces it, as a program attached to does,
    /// rather than dying with it.
    #[must_use]
    pub fn spawn_outliving_tracer(executable: &Path, arguments: &[String]) -> (Self, WaitStatus) {
        let mut command = std::process::Command::new(executable);
        command
            .args(arguments)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null());
        #[allow(
            unsafe_code,
            reason = "pre_exec is the only way to ask for tracing from the child"
        )]
        // SAFETY: after fork, the closure makes only async-signal-safe system
        // calls, and builds an error from a fixed errno, before Command execs.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut command, || {
                super::native::disable_address_randomization();
                nix::sys::ptrace::traceme()
                    .map_err(|errno| std::io::Error::from_raw_os_error(errno as i32))
            });
        }
        #[expect(
            clippy::zombie_processes,
            reason = "the tracee waits for its process with waitpid, and its drop reaps it"
        )]
        let child = command.spawn().expect("spawn a traced program");
        let tracee = Self {
            ptrace: super::native::LinuxPtrace::new(),
            pid: Pid::from_raw(i32::try_from(child.id()).expect("a process id fits i32")),
            held: Vec::new(),
            forked: Vec::new(),
            abandoned: false,
        };
        let first = tracee.wait(tracee.pid());
        (tracee, first)
    }

    /// The process, which is its first thread.
    #[must_use]
    pub const fn pid(&self) -> Tid {
        self.pid.as_raw()
    }

    /// Waits for `thread`'s next status, failing after ten seconds.
    #[must_use]
    pub fn wait(&self, thread: Tid) -> WaitStatus {
        let started = std::time::Instant::now();
        let event = loop {
            match super::native::wait_for(Pid::from_raw(thread), libc::__WALL | libc::WNOHANG) {
                Ok(Some(event)) => break event,
                Ok(None) | Err(Errno::EINTR) => {
                    let waited = started.elapsed();
                    assert!(
                        waited < std::time::Duration::from_secs(10),
                        "{thread} reported nothing for ten seconds"
                    );
                    // A single step reports within microseconds.
                    if waited < std::time::Duration::from_millis(1) {
                        std::thread::yield_now();
                    } else {
                        std::thread::sleep(std::time::Duration::from_micros(100));
                    }
                }
                Err(errno) => panic!("waiting for {thread} failed: {errno}"),
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

    /// Whether `thread` has a status to report now, which is not reaped.
    #[must_use]
    pub fn has_report(&self, thread: Tid) -> bool {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: `info` is a writable siginfo_t that outlives the call.
        #[allow(unsafe_code, reason = "nix's waitid cannot pass __WALL")]
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                thread.cast_unsigned(),
                info.as_mut_ptr(),
                libc::WEXITED | libc::WSTOPPED | libc::WNOHANG | libc::WNOWAIT | libc::__WALL,
            )
        };
        assert_eq!(result, 0, "waitid for {thread} failed");
        // SAFETY: waitid zeroes or fills the structure.
        #[allow(unsafe_code, reason = "reading what waitid wrote")]
        let pid = unsafe { info.assume_init().si_pid() };
        pid != 0
    }

    pub fn registers(&self, thread: Tid) -> std::result::Result<Registers, Errno> {
        let user = self
            .ptrace
            .registers(Pid::from_raw(thread))
            .map_err(errno_of)?;
        let mut registers = Registers::default();
        apply_user_registers(&mut registers, &user);
        registers.rflags = user.eflags;
        Ok(registers)
    }

    /// `orig_rax`: the system call a thread stopped inside, or -1.
    pub fn system_call(&self, thread: Tid) -> std::result::Result<u64, Errno> {
        self.ptrace
            .registers(Pid::from_raw(thread))
            .map(|user| user.orig_rax)
            .map_err(errno_of)
    }

    pub fn set_registers(
        &self,
        thread: Tid,
        registers: &Registers,
    ) -> std::result::Result<(), Errno> {
        let mut user = self
            .ptrace
            .registers(Pid::from_raw(thread))
            .map_err(errno_of)?;
        let flags = user.eflags;
        user = libc::user_regs_struct {
            orig_rax: user.orig_rax,
            ..user_registers(registers, crate::sim::kernel::NO_SYSTEM_CALL)
        };
        user.eflags = (flags & !SETTABLE_FLAGS) | (registers.rflags & SETTABLE_FLAGS);
        self.ptrace
            .set_registers(Pid::from_raw(thread), user)
            .map_err(errno_of)
    }

    /// `PTRACE_GETSIGINFO`, as the controller sees it.
    pub fn signal_view(
        &self,
        thread: Tid,
    ) -> std::result::Result<(i32, i32, Option<i32>, Option<u64>), Errno> {
        let raw = nix::sys::ptrace::getsiginfo(Pid::from_raw(thread))?;
        let metadata = super::native::signal_metadata(&raw);
        Ok((
            raw.si_signo,
            metadata.code,
            metadata.sender,
            metadata.fault_address,
        ))
    }

    pub fn event_message(&self, thread: Tid) -> std::result::Result<u64, Errno> {
        self.ptrace
            .event_message(Pid::from_raw(thread))
            .map(i64::cast_unsigned)
            .map_err(errno_of)
    }

    /// Leaves the process to its parent, as a tracer that exits does:
    /// nothing is killed or reaped. Returns the process.
    #[must_use]
    pub fn abandon(mut self) -> Tid {
        self.abandoned = true;
        self.pid()
    }

    /// Kills `child`, which the process forked, with it.
    pub fn adopt(&mut self, child: Tid) {
        self.forked.push(Pid::from_raw(child));
    }

    /// `PTRACE_SEIZE` with the options the controller seizes with, which
    /// leaves the thread running.
    pub fn seize(&self, thread: Tid) -> std::result::Result<(), Errno> {
        nix::sys::ptrace::seize(Pid::from_raw(thread), super::native::trace_options(false))
    }

    /// `PTRACE_INTERRUPT`.
    pub fn interrupt(&self, thread: Tid) -> std::result::Result<(), Errno> {
        nix::sys::ptrace::interrupt(Pid::from_raw(thread))
    }

    /// `PTRACE_DETACH`, delivering `signal`.
    pub fn detach(&self, thread: Tid, signal: Option<i32>) -> std::result::Result<(), Errno> {
        let signal =
            signal.map(|number| nix::sys::signal::Signal::try_from(number).expect("a real signal"));
        nix::sys::ptrace::detach(Pid::from_raw(thread), signal)
    }

    /// The children `thread` forked and its process has not reaped, as
    /// `/proc/<thread>/task/<thread>/children` lists them.
    #[must_use]
    pub fn children(&self, thread: Tid) -> Vec<Tid> {
        std::fs::read_to_string(format!("/proc/{thread}/task/{thread}/children"))
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|child| child.parse().ok())
            .collect()
    }

    pub fn set_options(&self, thread: Tid, exit_kill: bool) -> std::result::Result<(), Errno> {
        self.ptrace
            .set_options(Pid::from_raw(thread), exit_kill)
            .map_err(errno_of)
    }

    pub fn resume(
        &self,
        thread: Tid,
        signal: Option<i32>,
        single_step: bool,
    ) -> std::result::Result<(), Errno> {
        let signal = signal.map(|number| Signal::new(number).expect("a real signal"));
        if single_step {
            self.ptrace.step(Pid::from_raw(thread), signal)
        } else {
            self.ptrace
                .continue_execution(Pid::from_raw(thread), signal)
        }
        .map_err(errno_of)
    }

    /// `kill(2)` of the whole process.
    pub fn kill(&self, signal: i32) -> std::result::Result<(), Errno> {
        nix::sys::signal::kill(self.pid, nix::sys::signal::Signal::try_from(signal)?)
    }

    /// The tracer's `tgkill(SIGSTOP)` of one thread.
    pub fn request_stop(&self, thread: Tid) -> std::result::Result<(), Errno> {
        self.ptrace
            .request_stop(self.pid, Pid::from_raw(thread))
            .map_err(errno_of)
    }

    pub fn peek(&self, thread: Tid, address: u64) -> std::result::Result<u64, Errno> {
        nix::sys::ptrace::read(
            Pid::from_raw(thread),
            address as nix::sys::ptrace::AddressType,
        )
        .map(i64::cast_unsigned)
    }

    pub fn poke(&self, thread: Tid, address: u64, value: u64) -> std::result::Result<(), Errno> {
        self.ptrace
            .write_word(Pid::from_raw(thread), address, value)
            .map_err(errno_of)
    }

    /// `PTRACE_PEEKUSER` of a debug register.
    pub fn read_debug_register(
        &self,
        thread: Tid,
        index: usize,
    ) -> std::result::Result<u64, Errno> {
        self.ptrace
            .read_debug_register(Pid::from_raw(thread), index)
    }

    /// `PTRACE_POKEUSER` of a debug register.
    pub fn write_debug_register(
        &self,
        thread: Tid,
        index: usize,
        value: u64,
    ) -> std::result::Result<(), Errno> {
        self.ptrace
            .write_debug_register(Pid::from_raw(thread), index, value)
    }

    /// Holds `count` of `thread`'s hardware breakpoints with disabled perf
    /// breakpoints watching `address`, until the tracee is dropped.
    pub fn hold_debug_slots(&mut self, thread: Tid, count: usize, address: u64) {
        const PERF_TYPE_BREAKPOINT: u32 = 5;
        const HW_BREAKPOINT_W: u32 = 2;
        const DISABLED: u64 = 1;
        const EXCLUDE_KERNEL: u64 = 1 << 5;
        const EXCLUDE_HYPERVISOR: u64 = 1 << 6;
        let attributes = PerfEventAttr {
            kind: PERF_TYPE_BREAKPOINT,
            size: u32::try_from(std::mem::size_of::<PerfEventAttr>()).expect("small"),
            config: 0,
            sample_period: 0,
            sample_type: 0,
            read_format: 0,
            flags: DISABLED | EXCLUDE_KERNEL | EXCLUDE_HYPERVISOR,
            wakeup_events: 0,
            breakpoint_type: HW_BREAKPOINT_W,
            breakpoint_address: address,
            breakpoint_length: 8,
        };
        for _ in 0..count {
            // SAFETY: `attributes` is a valid, initialized perf_event_attr
            // of the size it declares, and outlives the call.
            #[allow(unsafe_code, reason = "perf_event_open has no safe wrapper")]
            let fd = unsafe {
                libc::syscall(
                    libc::SYS_perf_event_open,
                    std::ptr::from_ref(&attributes),
                    thread,
                    -1_i32,
                    -1_i32,
                    0_u64,
                )
            };
            assert!(
                fd >= 0,
                "perf_event_open failed: {}",
                std::io::Error::last_os_error()
            );
            // SAFETY: the kernel just returned this descriptor, which
            // nothing else owns.
            #[allow(unsafe_code, reason = "taking ownership of a new descriptor")]
            let fd = unsafe {
                <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(
                    i32::try_from(fd).expect("a descriptor fits i32"),
                )
            };
            self.held.push(fd);
        }
    }

    /// The thread group `/proc` names for `thread`, or `None` once it is
    /// gone.
    #[must_use]
    pub fn thread_group(&self, thread: Tid) -> Option<Tid> {
        match super::native::thread_group_id(Pid::from_raw(thread)) {
            Ok(group) => Some(group.as_raw()),
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("reading {thread}'s thread group failed: {error}"),
        }
    }

    /// The process's threads `/proc` lists, which includes zombies not yet
    /// reaped.
    #[must_use]
    pub fn threads(&self) -> Vec<Tid> {
        super::native::process_threads(self.pid)
            .map(|threads| threads.into_iter().map(Pid::as_raw).collect())
            .unwrap_or_default()
    }

    /// `/proc/<thread>/maps`, or `None` once it cannot be read.
    #[must_use]
    pub fn maps(&self, thread: Tid) -> Option<String> {
        std::fs::read_to_string(format!("/proc/{thread}/maps")).ok()
    }

    /// Reads the memory of `process`, the program or a child it forked,
    /// through `/proc/<pid>/mem`, or `None` where the kernel refuses.
    #[must_use]
    pub fn read_memory(&self, process: Tid, address: u64, length: usize) -> Option<Vec<u8>> {
        use std::os::unix::fs::FileExt as _;
        let file = std::fs::File::open(format!("/proc/{process}/mem")).ok()?;
        let mut bytes = vec![0; length];
        file.read_exact_at(&mut bytes, address).ok()?;
        Some(bytes)
    }
}

fn errno_of(error: Error) -> Errno {
    match error {
        Error::Backend(error) => match error.downcast_ref::<LinuxError>() {
            Some(LinuxError::System(errno)) => *errno,
            _ => panic!("not a system error: {error}"),
        },
        other => panic!("not a system error: {other}"),
    }
}

impl Drop for NativeTracee {
    fn drop(&mut self) {
        self.held.clear();
        if self.abandoned {
            return;
        }
        for &child in &self.forked {
            let _ = nix::sys::signal::kill(child, nix::sys::signal::Signal::SIGKILL);
            let started = std::time::Instant::now();
            // A child still traced is reaped here; a detached one is its
            // parent's to reap.
            while !matches!(
                super::native::wait_for(child, libc::__WALL | libc::WNOHANG),
                Ok(Some(WaitEvent::Exited(..) | WaitEvent::Signaled(..))) | Err(_)
            ) {
                let _ = self.ptrace.continue_execution(child, None);
                assert!(
                    started.elapsed() < std::time::Duration::from_secs(10),
                    "{child} outlived its test"
                );
                std::thread::yield_now();
            }
        }
        let _ = nix::sys::signal::kill(self.pid, nix::sys::signal::Signal::SIGKILL);
        // Reap every thread, so nothing outlives the test. A thread in a
        // stop, even one already reported such as its exit event, waits
        // there until it is continued, and the leader's exit is reported
        // only once every other thread is reaped.
        let started = std::time::Instant::now();
        loop {
            let mut threads = self.threads();
            if !threads.contains(&self.pid()) {
                threads.push(self.pid());
            }
            for thread in threads {
                let thread = Pid::from_raw(thread);
                let _ = self.ptrace.continue_execution(thread, None);
                match super::native::wait_for(thread, libc::__WALL | libc::WNOHANG) {
                    Ok(Some(WaitEvent::Exited(..) | WaitEvent::Signaled(..)))
                        if thread == self.pid =>
                    {
                        return;
                    }
                    Err(Errno::ECHILD) if thread == self.pid => return,
                    _ => {}
                }
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "{} outlived its test",
                self.pid
            );
            std::thread::yield_now();
        }
    }
}
