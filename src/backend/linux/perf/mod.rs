//! Sampling processes with perf events, from outside them: no ptrace and
//! no code in the program. One event per CPU follows a process and every
//! thread and child it starts, and writes samples, mappings, and thread
//! lifetimes to that CPU's ring.

pub mod decode;
mod sys;

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::{Command, ExitStatus};
use std::thread::JoinHandle;
use std::time::Duration;

use nix::libc;

use super::modules::{ModuleMapping, identify_mapped_module};

/// What makes a sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Every `period` of CPU time the program's threads spend in user code.
    Clock { period: Duration },
    /// Every execution of the instruction at `address`.
    Breakpoint { address: u64 },
}

/// How events sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventOptions {
    pub source: Source,
    /// Bytes of stack each sample copies, a multiple of eight.
    pub stack_bytes: u32,
    /// Data pages of each ring, a power of two.
    pub ring_pages: usize,
}

/// Why events could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error(
        "the kernel does not let this user sample the program (perf_event_paranoid is {paranoid}): {source}"
    )]
    Denied {
        paranoid: String,
        source: std::io::Error,
    },
    #[error("opening a perf event on CPU {cpu} failed: {source}")]
    Event { cpu: u32, source: std::io::Error },
    #[error("mapping a perf ring failed: {0}")]
    Ring(std::io::Error),
    #[error("reading the online CPUs failed: {0}")]
    Cpus(std::io::Error),
}

/// One event per online CPU, each with its ring.
#[derive(Debug)]
pub struct Events {
    rings: Vec<sys::Ring>,
    pages: usize,
}

/// The smallest rings worth sampling into.
const MIN_PAGES: usize = 4;

impl Events {
    /// Opens disabled events for `pid`, its threads, and its children,
    /// which enable themselves when it next executes a program.
    pub fn open(pid: u32, options: &EventOptions) -> Result<Self, OpenError> {
        use sys::flag;
        let mut attr = sys::Attr {
            size: u32::try_from(std::mem::size_of::<sys::Attr>()).expect("small"),
            sample_type: decode::SAMPLE_TYPE,
            flags: flag::DISABLED
                | flag::INHERIT
                | flag::EXCLUDE_KERNEL
                | flag::EXCLUDE_HV
                | flag::MMAP
                | flag::MMAP2
                | flag::COMM
                | flag::COMM_EXEC
                | flag::TASK
                | flag::ENABLE_ON_EXEC
                | flag::SAMPLE_ID_ALL
                | flag::EXCLUDE_CALLCHAIN_KERNEL
                | flag::USE_CLOCKID
                | flag::WATERMARK,
            wakeup_watermark: u32::try_from(options.ring_pages * sys::page_size() / 4)
                .unwrap_or(u32::MAX),
            sample_regs_user: decode::REGISTERS,
            sample_stack_user: options.stack_bytes,
            clockid: libc::CLOCK_MONOTONIC,
            sample_max_stack: 127,
            ..sys::Attr::default()
        };
        match options.source {
            Source::Clock { period } => {
                attr.kind = sys::TYPE_SOFTWARE;
                attr.config = sys::COUNT_SW_CPU_CLOCK;
                attr.sample_period = u64::try_from(period.as_nanos()).unwrap_or(u64::MAX).max(1);
            }
            Source::Breakpoint { address } => {
                attr.kind = sys::TYPE_BREAKPOINT;
                attr.bp_type = sys::BREAKPOINT_EXECUTE;
                attr.bp_addr = address;
                attr.bp_len = 8;
                attr.sample_period = 1;
            }
        }
        let pid = i32::try_from(pid).map_err(|_| OpenError::Cpus(std::io::Error::other("pid")))?;
        let mut fds = Vec::new();
        for cpu in online_cpus().map_err(OpenError::Cpus)? {
            let fd = sys::open(&attr, pid, i32::try_from(cpu).unwrap_or(i32::MAX)).map_err(
                |source| match source.raw_os_error() {
                    Some(libc::EACCES | libc::EPERM) => OpenError::Denied {
                        paranoid: std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
                            .map_or_else(|_| "unknown".into(), |text| text.trim().to_owned()),
                        source,
                    },
                    _ => OpenError::Event { cpu, source },
                },
            )?;
            fds.push(fd);
        }
        // The kernel lets a user lock only so much memory for rings, which
        // other recordings may be using: rings that do not fit are halved.
        let mut pages = options.ring_pages.next_power_of_two();
        loop {
            let rings: std::io::Result<Vec<sys::Ring>> = fds
                .iter()
                .map(|fd| sys::Ring::map(fd.try_clone()?, pages))
                .collect();
            match rings {
                Ok(rings) => {
                    record!(
                        "perf events for {pid}: {} CPUs, {pages} pages each",
                        rings.len()
                    );
                    return Ok(Self { rings, pages });
                }
                Err(error) if error.raw_os_error() == Some(libc::EPERM) && pages > MIN_PAGES => {
                    pages /= 2;
                }
                Err(error) => return Err(OpenError::Ring(error)),
            }
        }
    }

    /// Data pages each ring has.
    pub const fn pages(&self) -> usize {
        self.pages
    }

    /// Appends every ring's new records to `out`, a ring's whole records
    /// at a time, and returns how many bytes.
    pub fn drain(&mut self, out: &mut Vec<u8>) -> usize {
        self.rings.iter_mut().map(|ring| ring.drain(out)).sum()
    }

    /// Waits until a ring is a quarter full, or `timeout`.
    pub fn wait(&self, timeout: Duration) {
        sys::poll(&self.rings, timeout);
    }

    /// Stops every event sampling.
    pub fn disable(&self) {
        for ring in &self.rings {
            let _ = sys::enable(ring.fd(), false);
        }
    }
}

/// The CPUs `/sys` lists online, as `0-3,8` lists them.
fn online_cpus() -> std::io::Result<Vec<u32>> {
    let text = std::fs::read_to_string("/sys/devices/system/cpu/online")?;
    let mut cpus = Vec::new();
    for part in text.trim().split(',').filter(|part| !part.is_empty()) {
        let (first, last) = part.split_once('-').unwrap_or((part, part));
        let parse = |text: &str| {
            text.parse::<u32>()
                .map_err(|_| std::io::Error::other(format!("CPU list '{}'", text.trim())))
        };
        cpus.extend(parse(first)?..=parse(last)?);
    }
    Ok(cpus)
}

/// A program started for sampling.
#[derive(Debug)]
pub struct Launched {
    pub pid: u32,
    pub events: Events,
    /// Ends with the program's exit status, once it exits.
    pub exit: JoinHandle<std::io::Result<ExitStatus>>,
}

/// How to start the program.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LaunchOptions {
    /// Runs it at the addresses its image names, as breakpoint sources
    /// need to know where code is before it runs.
    pub no_address_randomization: bool,
}

/// Starts `command` with its events open before it executes, so the first
/// instruction of the program is sampled and nothing before it is.
///
/// The child waits between fork and exec until its events are open. A
/// thread of its own spawns it, because spawning returns only after the
/// exec, and stays to wait for its exit: the child dies with that thread.
pub fn launch(
    mut command: Command,
    options: &EventOptions,
    launch: LaunchOptions,
) -> Result<Launched, LaunchError> {
    let (pid_read, pid_write) = pipe()?;
    let (go_read, go_write) = pipe()?;
    gate(&mut command, [&pid_write, &go_read, &go_write], launch);
    let spawner = std::thread::Builder::new()
        .name("uscope-record-child".into())
        .spawn(move || {
            let spawned = command.spawn();
            // The child holds its own copies until it executes.
            drop((pid_write, go_read));
            spawned?.wait()
        })
        .map_err(LaunchError::Spawn)?;
    let mut pid = [0_u8; 4];
    if read_exact(&pid_read, &mut pid).is_err() {
        // The fork failed, or the child failed before it reached the gate.
        let error = match spawner.join() {
            Ok(Err(error)) => error,
            _ => std::io::Error::other("the program did not start"),
        };
        return Err(LaunchError::Spawn(error));
    }
    let pid = u32::from_le_bytes(pid);
    record!("perf launch: {pid} waits at its gate");
    let events = match Events::open(pid, options) {
        Ok(events) => events,
        Err(error) => {
            record!("perf launch: closing {pid}'s gate unopened: {error}");
            // Closing the gate unopened ends the child before it executes.
            drop(go_write);
            let _ = spawner.join();
            return Err(LaunchError::Events(error));
        }
    };
    write_all(&go_write, &[1])?;
    record!("perf launch: {pid} opened its gate");
    Ok(Launched {
        pid,
        events,
        exit: spawner,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("starting the program failed: {0}")]
    Spawn(#[from] std::io::Error),
    #[error(transparent)]
    Events(OpenError),
}

fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).map_err(std::io::Error::from)
}

fn read_exact(fd: &OwnedFd, buffer: &mut [u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::from(fd.try_clone()?);
    std::io::Read::read_exact(&mut file, buffer)
}

fn write_all(fd: &OwnedFd, buffer: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::from(fd.try_clone()?);
    std::io::Write::write_all(&mut file, buffer)
}

#[allow(
    unsafe_code,
    reason = "pre_exec is the only way to hold the child between fork and exec"
)]
fn gate(
    command: &mut Command,
    [pid_write, go_read, go_write]: [&OwnedFd; 3],
    launch: LaunchOptions,
) {
    use std::os::unix::process::CommandExt;
    let (pid_write, go_read, go_write) = (
        pid_write.as_raw_fd(),
        go_read.as_raw_fd(),
        go_write.as_raw_fd(),
    );
    let parent = std::process::id();
    // SAFETY: between fork and exec the closure calls only async-signal-safe
    // system calls on descriptors the child inherited, and builds errors
    // from fixed numbers.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != i32::try_from(parent).unwrap_or(i32::MAX) {
                return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
            }
            if launch.no_address_randomization {
                super::native::disable_address_randomization();
            }
            let pid = libc::getpid().to_le_bytes();
            if libc::write(pid_write, pid.as_ptr().cast(), pid.len()) != 4 {
                return Err(std::io::Error::from_raw_os_error(libc::EPIPE));
            }
            // Only the parent may hold the gate open, so that closing it
            // ends the wait.
            libc::close(go_write);
            let mut go = 0_u8;
            if libc::read(go_read, std::ptr::from_mut(&mut go).cast(), 1) != 1 {
                return Err(std::io::Error::from_raw_os_error(libc::ECANCELED));
            }
            Ok(())
        });
    }
}

/// The file an executable mapping maps and its image's load bias, if the
/// file at its path is still the one mapped.
pub fn identify(mmap: &decode::Mmap<'_>) -> Option<(PathBuf, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::OsStr::from_bytes(mmap.path);
    if !mmap.executable || !mmap.path.starts_with(b"/") {
        return None;
    }
    let deleted = mmap.path.ends_with(b" (deleted)");
    identify_mapped_module(&ModuleMapping {
        path: PathBuf::from(path),
        inode: mmap.inode?,
        start: mmap.start,
        file_offset: mmap.file_offset,
        deleted,
        executable: true,
    })
}
