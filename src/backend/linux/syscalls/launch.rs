//! Starting a program held between fork and exec.
//!
//! An engine then sees it from its first call: the BPF engine adds it to the
//! traced processes, and the ptrace engine seizes it, before it executes.

use std::os::fd::{AsRawFd as _, OwnedFd};
use std::process::{Command, ExitStatus};
use std::sync::mpsc;
use std::thread::JoinHandle;

use nix::libc;

use super::seccomp::Instruction;

/// A child released at its gate.
pub struct Gate {
    pub pid: u32,
    go: Option<OwnedFd>,
    spawner: Option<JoinHandle<std::io::Result<Option<ExitStatus>>>>,
    /// Dropping it lets a spawner that does not reap its child return.
    hold: Option<mpsc::Sender<()>>,
}

/// What the gate does once opened.
#[derive(Debug, Clone, Default)]
pub struct GateOptions {
    /// A seccomp filter the child installs before it executes.
    pub filter: Option<Vec<Instruction>>,
    /// Whether the spawning thread waits for the child and reaps it, as
    /// when nothing traces it. A ptrace engine reaps it itself.
    pub reap: bool,
}

impl Gate {
    /// Lets the child execute.
    pub fn open(&mut self) -> std::io::Result<()> {
        let go = self
            .go
            .take()
            .ok_or_else(|| std::io::Error::other("the gate is open"))?;
        let mut file = std::fs::File::from(go);
        std::io::Write::write_all(&mut file, &[1])?;
        record!("syscalls launch: {} opened its gate", self.pid);
        Ok(())
    }

    /// Waits for the spawning thread: the program's exit status when the
    /// thread reaped it, or why it never executed.
    pub fn finish(&mut self) -> std::io::Result<Option<ExitStatus>> {
        drop(self.hold.take());
        self.spawner.take().map_or(Ok(None), |spawner| {
            spawner
                .join()
                .unwrap_or_else(|_| Err(std::io::Error::other("the spawning thread panicked")))
        })
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        // A gate dropped unopened ends the child before it executes.
        drop(self.go.take());
        let _ = self.finish();
    }
}

fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).map_err(std::io::Error::from)
}

/// Starts `command` released at a gate. A thread of its own spawns it, because
/// spawning returns only after the exec, and stays while the child lives:
/// the child dies with that thread.
pub fn launch(mut command: Command, options: GateOptions) -> std::io::Result<Gate> {
    let (pid_read, pid_write) = pipe()?;
    let (go_read, go_write) = pipe()?;
    gate(
        &mut command,
        [&pid_write, &go_read, &go_write],
        options.filter,
    );
    let (hold, released) = mpsc::channel::<()>();
    let reap = options.reap;
    let spawner = std::thread::Builder::new()
        .name("uscope-syscalls-child".into())
        .spawn(move || {
            let spawned = command.spawn();
            drop((pid_write, go_read));
            let mut child = spawned?;
            if reap {
                return child.wait().map(Some);
            }
            // The tracer reaps the child; this thread only outlives it.
            let _ = released.recv();
            drop(child);
            Ok(None)
        })?;
    let mut pid = [0_u8; 4];
    let mut file = std::fs::File::from(pid_read);
    if std::io::Read::read_exact(&mut file, &mut pid).is_err() {
        drop(hold);
        let error = match spawner.join() {
            Ok(Err(error)) => error,
            _ => std::io::Error::other("the program did not start"),
        };
        return Err(error);
    }
    let pid = u32::from_le_bytes(pid);
    record!("syscalls launch: {pid} waits at its gate");
    Ok(Gate {
        pid,
        go: Some(go_write),
        spawner: Some(spawner),
        hold: Some(hold),
    })
}

#[allow(
    unsafe_code,
    reason = "pre_exec is the only way to hold the child between fork and exec"
)]
fn gate(
    command: &mut Command,
    [pid_write, go_read, go_write]: [&OwnedFd; 3],
    filter: Option<Vec<Instruction>>,
) {
    use std::os::unix::process::CommandExt;
    let (pid_write, go_read, go_write) = (
        pid_write.as_raw_fd(),
        go_read.as_raw_fd(),
        go_write.as_raw_fd(),
    );
    let parent = std::process::id();
    // Built before the fork: the child may not allocate.
    let filter: Option<Vec<libc::sock_filter>> = filter.map(|program| {
        program
            .into_iter()
            .map(|instruction| libc::sock_filter {
                code: instruction.code,
                jt: instruction.jt,
                jf: instruction.jf,
                k: instruction.k,
            })
            .collect()
    });
    // SAFETY: between fork and exec the closure calls only async-signal-safe
    // system calls on descriptors the child inherited and on the filter
    // built before the fork, and builds errors from fixed numbers.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != i32::try_from(parent).unwrap_or(i32::MAX) {
                return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
            }
            let pid = libc::getpid().to_le_bytes();
            if libc::write(pid_write, pid.as_ptr().cast(), pid.len()) != 4 {
                return Err(std::io::Error::from_raw_os_error(libc::EPIPE));
            }
            libc::close(go_write);
            let mut go = 0_u8;
            if libc::read(go_read, std::ptr::from_mut(&mut go).cast(), 1) != 1 {
                return Err(std::io::Error::from_raw_os_error(libc::ECANCELED));
            }
            if let Some(filter) = &filter {
                // An unprivileged process may install a filter only once
                // it can gain no privileges by executing.
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                let program = libc::sock_fprog {
                    len: u16::try_from(filter.len()).unwrap_or(u16::MAX),
                    filter: filter.as_ptr().cast_mut(),
                };
                if libc::syscall(
                    libc::SYS_seccomp,
                    libc::SECCOMP_SET_MODE_FILTER,
                    0,
                    std::ptr::from_ref(&program),
                ) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}
