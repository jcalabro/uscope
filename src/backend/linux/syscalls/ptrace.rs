//! The ptrace engine's tracer for `uscope syscalls --engine ptrace`: exact
//! and synchronous, for programs it launches or, at a high price, attaches
//! to.
//!
//! A launched program installs a seccomp filter before it executes, so
//! only the chosen calls stop, at a `PTRACE_EVENT_SECCOMP` stop at entry;
//! resuming with `PTRACE_SYSCALL` brings the call's exit stop. Every other
//! call runs at full speed. A filter outlives its tracer, and an untraced
//! process whose filter asks for a tracer fails the chosen calls with
//! `ENOSYS`, so every descendant stays traced and dies with the tracer.
//!
//! An attached process has no filter, so every call of every thread stops
//! twice. The caller asks for that explicitly.
//!
//! One thread, the one that calls [`trace`], makes every ptrace request,
//! as AGENTS.md requires; a waiter thread only waits and passes statuses
//! on.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nix::errno::Errno;
use nix::libc;
use nix::sys::ptrace::{self, Options};
use nix::unistd::Pid;

use super::super::native::{ptrace_with_signal, wait_for};
use super::super::signals::{Signal, WaitEvent};
use super::launch::{self, Gate, GateOptions};
use super::{Sink, seccomp};
use crate::syscalls::captures::{self, Limits, Memory};
use crate::syscalls::specs::{self, Ret};
use crate::syscalls::table::X32_SYSCALL_BIT;
use crate::syscalls::{
    Abi, CallSet, Capture, Device, EventFlags, InFlight, Outcome, ProcessEnd, ReadAt, SyscallEvent,
    SyscallId, TraceEvent,
};

/// What to trace and how much to copy.
#[derive(Debug, Clone)]
pub struct TraceOptions {
    pub calls: CallSet,
    pub limits: Limits,
    /// Whole environments, which otherwise only count.
    pub verbose: bool,
    /// Reports calls of the processes the targets create, not only theirs.
    pub follow: bool,
}

/// What to trace.
pub enum Target {
    /// A program to start, filtered from its first call.
    Launch(Command),
    /// Running processes, every call of which stops.
    Attach(Vec<u32>),
}

#[derive(Debug, thiserror::Error)]
pub enum TraceError {
    #[error("starting the program failed: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("could not attach to process {pid}: {errno}")]
    Attach { pid: u32, errno: Errno },
    #[error("tracing failed: {0}")]
    System(#[from] Errno),
}

/// How tracing ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finished {
    /// How the first target ended, when it did while traced.
    pub end: Option<ProcessEnd>,
}

const OP_ENTRY: u8 = 1;
const OP_EXIT: u8 = 2;
const OP_SECCOMP: u8 = 3;
const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;

/// What `PTRACE_GET_SYSCALL_INFO` says about a stopped thread.
#[derive(Debug, Clone, Copy)]
struct SyscallInfo {
    op: u8,
    arch: u32,
    /// For an entry or seccomp stop.
    number: u64,
    args: [u64; 6],
    /// For an exit stop.
    value: i64,
}

#[allow(
    unsafe_code,
    reason = "reading the info as bytes avoids the libc struct's union"
)]
fn syscall_info(pid: Pid) -> Result<SyscallInfo, Errno> {
    let mut buffer = [0_u8; 88];
    // SAFETY: the kernel writes at most `buffer.len()` bytes, the size the
    // request passes, into the buffer, which outlives the call.
    let written = unsafe {
        libc::ptrace(
            libc::PTRACE_GET_SYSCALL_INFO,
            pid.as_raw(),
            std::ptr::without_provenance_mut::<libc::c_void>(buffer.len()),
            buffer.as_mut_ptr().cast::<libc::c_void>(),
        )
    };
    Errno::result(written)?;
    let word = |at: usize| u64::from_le_bytes(buffer[at..at + 8].try_into().unwrap_or_default());
    let mut args = [0; 6];
    for (index, arg) in args.iter_mut().enumerate() {
        *arg = word(32 + index * 8);
    }
    Ok(SyscallInfo {
        op: buffer[0],
        arch: u32::from_le_bytes(buffer[4..8].try_into().unwrap_or_default()),
        number: word(24),
        args,
        value: word(24).cast_signed(),
    })
}

#[allow(
    unsafe_code,
    reason = "reading the siginfo as bytes keeps every field the kernel wrote"
)]
fn siginfo(pid: Pid) -> Option<[u8; 128]> {
    let mut buffer = [0_u8; 128];
    // SAFETY: PTRACE_GETSIGINFO writes one siginfo_t, 128 bytes on every
    // Linux architecture, into the buffer, which outlives the call.
    let read = unsafe {
        libc::ptrace(
            libc::PTRACE_GETSIGINFO,
            pid.as_raw(),
            std::ptr::null_mut::<libc::c_void>(),
            buffer.as_mut_ptr().cast::<libc::c_void>(),
        )
    };
    Errno::result(read).ok().map(|_| buffer)
}

/// `CLOCK_MONOTONIC` nanoseconds, the clock every engine's events use.
#[must_use]
pub fn monotonic() -> u64 {
    nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).map_or(0, |time| {
        u64::try_from(time.tv_sec()).unwrap_or(0) * 1_000_000_000
            + u64::try_from(time.tv_nsec()).unwrap_or(0)
    })
}

/// The traced process's memory, read from outside it.
struct ProcessMemory(u32);

impl Memory for ProcessMemory {
    fn read(&mut self, address: u64, buffer: &mut [u8]) -> std::io::Result<usize> {
        super::super::perf::read_memory(self.0, address, buffer)
    }
}

/// A process's thread group, from `/proc`.
fn tgid_of(tid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{tid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Tgid:"))?
        .trim()
        .parse()
        .ok()
}

/// What `/proc/PID/fd/N` links to, and whether it closes on exec.
fn descriptor(process: u32, fd: i64, time: u64, made: bool) -> Option<TraceEvent> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    if fd == AT_FDCWD {
        let link = std::fs::read_link(format!("/proc/{process}/cwd")).ok()?;
        return Some(TraceEvent::Descriptor {
            process,
            fd,
            time,
            text: link.to_string_lossy().into(),
            cloexec: false,
            device: None,
            made,
        });
    }
    let path = format!("/proc/{process}/fd/{fd}");
    let link = std::fs::read_link(&path).ok()?;
    let flags = std::fs::read_to_string(format!("/proc/{process}/fdinfo/{fd}")).ok();
    let cloexec = flags
        .as_deref()
        .and_then(|info| info.lines().find_map(|line| line.strip_prefix("flags:")))
        .and_then(|flags| u64::from_str_radix(flags.trim(), 8).ok())
        .is_some_and(|flags| flags & 0o2_000_000 != 0);
    let device = std::fs::metadata(&path).ok().and_then(|metadata| {
        let kind = metadata.file_type();
        (kind.is_char_device() || kind.is_block_device()).then(|| Device {
            block: kind.is_block_device(),
            major: nix::sys::stat::major(metadata.rdev())
                .try_into()
                .unwrap_or(0),
            minor: nix::sys::stat::minor(metadata.rdev())
                .try_into()
                .unwrap_or(0),
        })
    });
    Some(TraceEvent::Descriptor {
        process,
        fd,
        time,
        text: link.to_string_lossy().into(),
        cloexec,
        device,
        made,
    })
}

/// What the descriptors, or the working directory, a call just made refer
/// to, read while they exist.
fn made(
    process: u32,
    call: SyscallId,
    outcome: Outcome,
    captured: &[Capture],
    time: u64,
) -> Vec<TraceEvent> {
    let mut fds = Vec::new();
    match outcome {
        Outcome::Returned(fd) if specs::spec(call).is_some_and(|spec| spec.ret == Ret::Fd) => {
            fds.push(fd);
        }
        Outcome::Returned(0) if matches!(call.name(), Some("chdir" | "fchdir")) => {
            fds.push(AT_FDCWD);
        }
        Outcome::Returned(0) if matches!(call.name(), Some("pipe" | "pipe2" | "socketpair")) => {
            if let Some(pair) = captured.iter().find(|capture| capture.when == ReadAt::Exit) {
                for at in [0, 4] {
                    let fd = pair
                        .bytes
                        .get(at..at + 4)
                        .and_then(|bytes| bytes.try_into().ok());
                    fds.extend(fd.map(|fd| i64::from(i32::from_le_bytes(fd))));
                }
            }
        }
        _ => {}
    }
    fds.into_iter()
        .filter_map(|fd| descriptor(process, fd, time, true))
        .collect()
}

/// The descriptor number the working directory is reported under.
const AT_FDCWD: i64 = -100;

/// A call seen at entry, waiting for its exit.
#[derive(Debug)]
struct Entry {
    call: SyscallId,
    args: [u64; 6],
    entered: u64,
    captured: Vec<Capture>,
    /// Whether the selection chose it; an attached thread stops at every
    /// call.
    chosen: bool,
}

#[derive(Debug)]
struct Thread {
    process: u32,
    pending: Option<Entry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Chosen calls stop at a seccomp stop.
    Filtered,
    /// Every call stops.
    EveryCall,
}

struct Tracer<'a> {
    mode: Mode,
    options: &'a TraceOptions,
    threads: BTreeMap<u32, Thread>,
    /// Processes whose calls are reported.
    reported: BTreeSet<u32>,
    first: u32,
    end: Option<ProcessEnd>,
    detaching: bool,
    sink: &'a mut dyn Sink,
}

const fn call_id(info: &SyscallInfo) -> SyscallId {
    #[expect(clippy::cast_possible_truncation, reason = "call numbers are small")]
    let number = info.number as u32;
    if info.arch == AUDIT_ARCH_X86_64 {
        if number & X32_SYSCALL_BIT != 0 {
            SyscallId::new(Abi::X32, number & !X32_SYSCALL_BIT)
        } else {
            SyscallId::new(Abi::X86_64, number)
        }
    } else {
        SyscallId::new(Abi::X86_32, number)
    }
}

impl Tracer<'_> {
    fn emit(&mut self, event: TraceEvent) {
        self.sink.event(event);
    }

    /// Reports the chosen calls threads have been blocked in for longer
    /// than the sink asks about.
    fn report_in_flight(&mut self) {
        let Some(after) = self.sink.in_flight_after() else {
            return;
        };
        let now = monotonic();
        let calls: Vec<InFlight> = self
            .threads
            .iter()
            .filter(|(_, thread)| self.reports(thread.process))
            .filter_map(|(&tid, thread)| {
                let entry = thread.pending.as_ref().filter(|entry| entry.chosen)?;
                (now.saturating_sub(entry.entered) >= after).then_some(InFlight {
                    thread: tid,
                    process: thread.process,
                    call: entry.call,
                    args: entry.args,
                    entered: entry.entered,
                })
            })
            .collect();
        self.sink.in_flight(&calls);
    }

    fn reports(&self, process: u32) -> bool {
        self.options.follow || self.reported.contains(&process)
    }

    /// Resumes a stopped thread: to the exit of the call it is in, or on.
    fn resume(&mut self, tid: u32, signal: Option<Signal>) -> Result<(), Errno> {
        let pid = Pid::from_raw(tid.cast_signed());
        if self.detaching {
            return self.detach(tid, signal);
        }
        let request = match self.threads.get(&tid) {
            Some(thread) if thread.pending.is_some() => libc::PTRACE_SYSCALL,
            _ if self.mode == Mode::EveryCall => libc::PTRACE_SYSCALL,
            _ => libc::PTRACE_CONT,
        };
        record!("syscalls ptrace: resume {tid} with {request} and {signal:?}");
        match ptrace_with_signal(request, pid, signal) {
            // A thread killed meanwhile reports its exit next.
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn detach(&mut self, tid: u32, signal: Option<Signal>) -> Result<(), Errno> {
        record!("syscalls ptrace: detach {tid}");
        if let Some(thread) = self.threads.remove(&tid) {
            self.unfinished(tid, thread, Outcome::Unfinished);
        }
        match ptrace_with_signal(
            libc::PTRACE_DETACH,
            Pid::from_raw(tid.cast_signed()),
            signal,
        ) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Reports a call whose exit will never be seen.
    fn unfinished(&mut self, tid: u32, thread: Thread, outcome: Outcome) {
        let Some(entry) = thread.pending.filter(|entry| entry.chosen) else {
            return;
        };
        if !self.reports(thread.process) {
            return;
        }
        let ends = entry
            .call
            .name()
            .is_some_and(|name| matches!(name, "exit" | "exit_group" | "execve" | "execveat"));
        let outcome = if ends && outcome == Outcome::Unfinished {
            Outcome::ProcessExited
        } else {
            outcome
        };
        self.emit(TraceEvent::Call(SyscallEvent {
            thread: tid,
            process: thread.process,
            call: entry.call,
            args: entry.args,
            captured: entry.captured,
            outcome,
            entered: Some(entry.entered),
            exited: None,
            stack: None,
            flags: EventFlags::default(),
        }));
    }

    fn thread(&mut self, tid: u32) -> &mut Thread {
        self.threads.entry(tid).or_insert_with(|| Thread {
            process: tgid_of(tid).unwrap_or(tid),
            pending: None,
        })
    }

    fn entry(&mut self, tid: u32, info: &SyscallInfo) -> Result<(), Errno> {
        let entered = monotonic();
        let call = call_id(info);
        let chosen = self.options.calls.contains(call);
        let process = self.thread(tid).process;
        let captured = if chosen && self.reports(process) {
            captures::capture(
                call,
                &info.args,
                ReadAt::Entry,
                None,
                self.options.limits,
                self.options.verbose,
                &mut ProcessMemory(tid),
            )
        } else {
            Vec::new()
        };
        let thread = self.thread(tid);
        if let Some(previous) = thread.pending.take() {
            let stale = Thread {
                process: thread.process,
                pending: Some(previous),
            };
            self.unfinished(tid, stale, Outcome::Unavailable);
        }
        self.thread(tid).pending = Some(Entry {
            call,
            args: info.args,
            entered,
            captured,
            chosen,
        });
        self.resume(tid, None)
    }

    fn exit(&mut self, tid: u32, info: &SyscallInfo) -> Result<(), Errno> {
        let exited = monotonic();
        let outcome = Outcome::of_return(info.value);
        let thread = self.thread(tid);
        let process = thread.process;
        let pending = thread.pending.take();
        let (call, args, entered, mut captured, flags, chosen) = if let Some(entry) = pending {
            (
                entry.call,
                entry.args,
                Some(entry.entered),
                entry.captured,
                EventFlags::default(),
                entry.chosen,
            )
        } else {
            // The exit of a call whose entry came before tracing did: the
            // registers still hold its number and arguments.
            let Ok(registers) = ptrace::getregs(Pid::from_raw(tid.cast_signed())) else {
                return self.resume(tid, None);
            };
            #[expect(clippy::cast_possible_truncation, reason = "call numbers are small")]
            let number = registers.orig_rax as u32;
            let (call, args) = if registers.cs == 0x23 {
                let args = [
                    registers.rbx,
                    registers.rcx,
                    registers.rdx,
                    registers.rsi,
                    registers.rdi,
                    registers.rbp,
                ];
                (
                    SyscallId::new(Abi::X86_32, number),
                    args.map(|word| word & 0xffff_ffff),
                )
            } else {
                let abi = if number & X32_SYSCALL_BIT == 0 {
                    Abi::X86_64
                } else {
                    Abi::X32
                };
                let args = [
                    registers.rdi,
                    registers.rsi,
                    registers.rdx,
                    registers.r10,
                    registers.r8,
                    registers.r9,
                ];
                (SyscallId::new(abi, number & !X32_SYSCALL_BIT), args)
            };
            let chosen = self.options.calls.contains(call);
            (
                call,
                args,
                None,
                Vec::new(),
                EventFlags::ENTRY_UNSEEN,
                chosen,
            )
        };
        if chosen && self.reports(process) {
            captured.extend(captures::capture(
                call,
                &args,
                ReadAt::Exit,
                Some(outcome),
                self.options.limits,
                self.options.verbose,
                &mut ProcessMemory(tid),
            ));
            for event in made(process, call, outcome, &captured, exited) {
                self.emit(event);
            }
            self.emit(TraceEvent::Call(SyscallEvent {
                thread: tid,
                process,
                call,
                args,
                captured,
                outcome,
                entered,
                exited: Some(exited),
                stack: None,
                flags,
            }));
        }
        self.resume(tid, None)
    }

    fn syscall_stop(&mut self, tid: u32, seccomp: bool) -> Result<(), Errno> {
        let info = match syscall_info(Pid::from_raw(tid.cast_signed())) {
            Ok(info) => info,
            Err(Errno::ESRCH) => return Ok(()),
            Err(error) => return Err(error),
        };
        record!(
            "syscalls ptrace: {tid} stopped with op {} for {}",
            info.op,
            info.number
        );
        let _ = seccomp;
        match info.op {
            // A filtered call stops once at its seccomp stop; were an entry
            // stop to follow, it is the same call.
            OP_ENTRY
                if self
                    .threads
                    .get(&tid)
                    .and_then(|thread| thread.pending.as_ref())
                    .is_some_and(|entry| entry.call == call_id(&info)) =>
            {
                self.resume(tid, None)
            }
            OP_SECCOMP | OP_ENTRY => self.entry(tid, &info),
            OP_EXIT => self.exit(tid, &info),
            _ => self.resume(tid, None),
        }
    }

    fn event(&mut self, tid: u32, signal: Signal, event: i32) -> Result<(), Errno> {
        let pid = Pid::from_raw(tid.cast_signed());
        match event {
            libc::PTRACE_EVENT_SECCOMP => self.syscall_stop(tid, true),
            libc::PTRACE_EVENT_FORK | libc::PTRACE_EVENT_VFORK | libc::PTRACE_EVENT_CLONE => {
                let parent = self.thread(tid).process;
                if let Ok(child) = ptrace::getevent(pid) {
                    let child = u32::try_from(child).unwrap_or(0);
                    let process = tgid_of(child).unwrap_or(child);
                    let thread = process == parent;
                    self.threads.entry(child).or_insert(Thread {
                        process,
                        pending: None,
                    });
                    if self.reports(parent) {
                        if !thread {
                            self.reported.insert(process);
                        }
                        self.emit(TraceEvent::Spawn {
                            parent_thread: tid,
                            parent,
                            child,
                            thread,
                            time: monotonic(),
                        });
                    }
                }
                self.resume(tid, None)
            }
            libc::PTRACE_EVENT_EXEC => {
                // A thread other than the leader that executes takes the
                // leader's id.
                if let Ok(former) = ptrace::getevent(pid)
                    && let Ok(former) = u32::try_from(former)
                    && former != tid
                    && let Some(moved) = self.threads.remove(&former)
                {
                    self.threads.insert(tid, moved);
                }
                let process = self.thread(tid).process;
                if self.reports(process) {
                    let path = std::fs::read_link(format!("/proc/{tid}/exe"))
                        .map_or_else(|_| "?".into(), |path| path.to_string_lossy().into());
                    self.emit(TraceEvent::Exec {
                        thread: tid,
                        process,
                        time: monotonic(),
                        path,
                    });
                }
                self.resume(tid, None)
            }
            libc::PTRACE_EVENT_STOP => {
                self.thread(tid);
                if is_stopping(signal) && !self.detaching {
                    // A group-stop: the thread stays stopped, as it would
                    // untraced, until a SIGCONT.
                    record!("syscalls ptrace: listen {tid}");
                    return match ptrace_with_signal(libc::PTRACE_LISTEN, pid, None) {
                        Ok(()) | Err(Errno::ESRCH) => Ok(()),
                        Err(error) => Err(error),
                    };
                }
                self.resume(tid, None)
            }
            _ => self.resume(tid, None),
        }
    }

    fn signal(&mut self, tid: u32, signal: Signal) -> Result<(), Errno> {
        let process = self.thread(tid).process;
        if self.reports(process) && !self.detaching {
            let info = siginfo(Pid::from_raw(tid.cast_signed()));
            let word = |at: usize| {
                info.and_then(|info| {
                    Some(i32::from_le_bytes(info.get(at..at + 4)?.try_into().ok()?))
                })
            };
            let code = word(8);
            // Processes send with SI_USER, SI_QUEUE, SI_TKILL, and the like.
            let sender = code
                .filter(|&code| code <= 0 && code != -2)
                .and_then(|_| word(16))
                .and_then(|pid| u32::try_from(pid).ok());
            self.emit(TraceEvent::Signal {
                thread: tid,
                process,
                time: monotonic(),
                signal: signal.number(),
                code,
                sender,
                info: info.map(|info| Arc::from(&info[..])),
            });
        }
        self.resume(tid, Some(signal))
    }

    fn ended(&mut self, tid: u32, end: ProcessEnd) {
        let Some(thread) = self.threads.remove(&tid) else {
            return;
        };
        let process = thread.process;
        self.unfinished(tid, thread, Outcome::Unfinished);
        if tid != process {
            return;
        }
        // A leader's exit comes after every other thread's: the process is
        // gone.
        let orphans: Vec<u32> = self
            .threads
            .iter()
            .filter(|(_, thread)| thread.process == process)
            .map(|(&tid, _)| tid)
            .collect();
        for orphan in orphans {
            if let Some(thread) = self.threads.remove(&orphan) {
                self.unfinished(orphan, thread, Outcome::Unfinished);
            }
        }
        if process == self.first {
            self.end = Some(end);
        }
        if self.reports(process) {
            self.emit(TraceEvent::Exit {
                process,
                time: monotonic(),
                end,
            });
        }
    }

    fn handle(&mut self, event: WaitEvent) -> Result<(), Errno> {
        match event {
            WaitEvent::Exited(pid, code) => {
                self.ended(pid.as_raw().cast_unsigned(), ProcessEnd::Exited(code));
                Ok(())
            }
            WaitEvent::Signaled(pid, signal, core) => {
                self.ended(
                    pid.as_raw().cast_unsigned(),
                    ProcessEnd::Killed {
                        signal: signal.number(),
                        core,
                    },
                );
                Ok(())
            }
            WaitEvent::PtraceSyscall(pid) => self.syscall_stop(pid.as_raw().cast_unsigned(), false),
            WaitEvent::PtraceEvent(pid, signal, event) => {
                self.event(pid.as_raw().cast_unsigned(), signal, event)
            }
            WaitEvent::Stopped(pid, signal) => self.signal(pid.as_raw().cast_unsigned(), signal),
            WaitEvent::Continued(_) => Ok(()),
        }
    }

    /// Stops tracing attached processes: every thread is interrupted, and
    /// detached at its next stop.
    fn begin_detach(&mut self) {
        self.detaching = true;
        let threads: Vec<u32> = self.threads.keys().copied().collect();
        for tid in threads {
            record!("syscalls ptrace: interrupt {tid} to detach");
            let _ = ptrace::interrupt(Pid::from_raw(tid.cast_signed()));
        }
    }
}

const fn is_stopping(signal: Signal) -> bool {
    matches!(
        signal.number(),
        libc::SIGSTOP | libc::SIGTSTP | libc::SIGTTIN | libc::SIGTTOU
    )
}

fn trace_options(launched: bool, follow: bool) -> Options {
    let mut options =
        Options::PTRACE_O_TRACESYSGOOD | Options::PTRACE_O_TRACECLONE | Options::PTRACE_O_TRACEEXEC;
    // A filtered process's children inherit its filter, so they stay
    // traced whether or not their calls are reported.
    if launched || follow {
        options |= Options::PTRACE_O_TRACEFORK | Options::PTRACE_O_TRACEVFORK;
    }
    if launched {
        options |= Options::PTRACE_O_TRACESECCOMP | Options::PTRACE_O_EXITKILL;
    }
    options
}

/// Seizes every thread of a process, until listing its threads finds no
/// new one.
fn seize_process(pid: u32, options: Options, seized: &mut BTreeSet<u32>) -> Result<(), TraceError> {
    loop {
        let entries =
            std::fs::read_dir(format!("/proc/{pid}/task")).map_err(|_| TraceError::Attach {
                pid,
                errno: Errno::ESRCH,
            })?;
        let mut new = false;
        for entry in entries.flatten() {
            let Some(tid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            if !seized.insert(tid) {
                continue;
            }
            new = true;
            record!("syscalls ptrace: seize {tid}");
            match ptrace::seize(Pid::from_raw(tid.cast_signed()), options) {
                Ok(()) => {
                    // Every-call tracing starts at the thread's next stop.
                    let _ = ptrace::interrupt(Pid::from_raw(tid.cast_signed()));
                }
                // A thread that exited since it was listed.
                Err(Errno::ESRCH) if tid != pid => {}
                Err(errno) => return Err(TraceError::Attach { pid, errno }),
            }
        }
        if !new {
            return Ok(());
        }
    }
}

/// Traces `target` until every traced process ends, or until `stop` is set,
/// which detaches attached processes and kills launched ones. Each event
/// goes to `sink` as it happens.
pub fn trace(
    target: Target,
    options: &TraceOptions,
    stop: &AtomicBool,
    sink: &mut dyn Sink,
) -> Result<Finished, TraceError> {
    let mut gate: Option<Gate> = None;
    let (mode, first) = match target {
        Target::Launch(command) => {
            let filter = seccomp::filter(&options.calls);
            let mut launched = launch::launch(
                command,
                GateOptions {
                    filter: Some(filter),
                    reap: false,
                },
            )
            .map_err(TraceError::Spawn)?;
            let pid = launched.pid;
            record!("syscalls ptrace: seize launched {pid}");
            ptrace::seize(
                Pid::from_raw(pid.cast_signed()),
                trace_options(true, options.follow),
            )
            .map_err(|errno| TraceError::Attach { pid, errno })?;
            launched.open().map_err(TraceError::Spawn)?;
            gate = Some(launched);
            (Mode::Filtered, pid)
        }
        Target::Attach(pids) => {
            let mut seized = BTreeSet::new();
            for &pid in &pids {
                seize_process(pid, trace_options(false, options.follow), &mut seized)?;
                sink.attached(pid);
            }
            (Mode::EveryCall, pids.first().copied().unwrap_or(0))
        }
    };
    let mut tracer = Tracer {
        mode,
        options,
        threads: BTreeMap::new(),
        reported: BTreeSet::from([first]),
        first,
        end: None,
        detaching: false,
        sink,
    };
    let result = run(&mut tracer, stop, gate.is_some());
    // A launched program dies with its tracer; the filter would otherwise
    // fail its chosen calls.
    if gate.is_some() && result.is_err() {
        for &tid in tracer.threads.keys() {
            let _ = nix::sys::signal::kill(
                Pid::from_raw(tid.cast_signed()),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
    let spawn = gate.as_mut().map(Gate::finish);
    let finished = result?;
    if finished.end.is_none()
        && let Some(Err(error)) = spawn
    {
        return Err(TraceError::Spawn(error));
    }
    Ok(finished)
}

/// The first wait after a status, and the longest while nothing happens.
/// The tracer polls rather than blocks: a blocked wait outlives the last
/// tracee's detach, which wakes no waiter.
const SHORTEST_POLL: Duration = Duration::from_micros(20);
const LONGEST_POLL: Duration = Duration::from_millis(5);

fn run(tracer: &mut Tracer<'_>, stop: &AtomicBool, launched: bool) -> Result<Finished, TraceError> {
    let mut stopping = false;
    let mut reported = monotonic();
    let mut poll = SHORTEST_POLL;
    loop {
        if monotonic() - reported > 100_000_000 {
            tracer.report_in_flight();
            reported = monotonic();
        }
        if stop.load(Ordering::Relaxed) && !stopping {
            stopping = true;
            if launched {
                // Killing every traced process ends the trace.
                for &tid in tracer.threads.keys() {
                    let _ = nix::sys::signal::kill(
                        Pid::from_raw(tid.cast_signed()),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                }
                let _ = nix::sys::signal::kill(
                    Pid::from_raw(tracer.first.cast_signed()),
                    nix::sys::signal::Signal::SIGKILL,
                );
            } else {
                tracer.begin_detach();
                if tracer.threads.is_empty() {
                    break;
                }
            }
        }
        match wait_for(Pid::from_raw(-1), libc::__WALL | libc::WNOHANG) {
            Ok(Some(event)) => {
                poll = SHORTEST_POLL;
                tracer.handle(event)?;
                if tracer.detaching && tracer.threads.is_empty() {
                    break;
                }
            }
            Ok(None) => {
                if tracer.detaching && tracer.threads.is_empty() {
                    break;
                }
                std::thread::sleep(poll);
                poll = (poll * 2).min(LONGEST_POLL);
            }
            Err(Errno::EINTR) => {}
            Err(Errno::ECHILD) => break,
            Err(error) => return Err(TraceError::System(error)),
        }
    }
    // Calls still in progress when the trace ended.
    let threads = std::mem::take(&mut tracer.threads);
    for (tid, thread) in threads {
        tracer.unfinished(tid, thread, Outcome::Unfinished);
    }
    Ok(Finished { end: tracer.end })
}

/// Reports every descriptor a process holds, as tracing begins.
pub fn seed_descriptors(process: u32, sink: &mut dyn Sink) {
    let Ok(entries) = std::fs::read_dir(format!("/proc/{process}/fd")) else {
        return;
    };
    let time = monotonic();
    let mut fds: Vec<i64> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .collect();
    fds.sort_unstable();
    for fd in std::iter::once(AT_FDCWD).chain(fds) {
        if let Some(event) = descriptor(process, fd, time, false) {
            sink.event(event);
        }
    }
}
