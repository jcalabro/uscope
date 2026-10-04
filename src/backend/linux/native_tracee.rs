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
        let tracee = Self { ptrace, pid };
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
