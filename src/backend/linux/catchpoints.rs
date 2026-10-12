//! System-call catchpoints. While any exists, every thread resumes to stop
//! at each of its calls' entries and exits; a call a catchpoint catches,
//! whose condition holds, stops every thread, and any other call resumes
//! at once.

use std::sync::Arc;

use nix::unistd::Pid;

use std::collections::BTreeMap;

use super::memory::read_logical_memory;
use super::native::LinuxTraceOps;
use super::syscalls::info::{self, OP_ENTRY, OP_EXIT};
use super::{BreakpointSite, Controller, NativeThreadState};
use crate::protocol::{ConditionOwner, DebuggerEvent, StopReason, SyscallStop};
use crate::syscalls::captures::{self, Limits};
use crate::syscalls::specs::{self, Arg};
use crate::syscalls::{Capture, CaptureState, Outcome, ReadAt, SyscallId};
use crate::{CatchPhase, Catchpoint, CatchpointId, Condition, Error, Result, VirtualAddress};

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn add_catchpoint(
        &mut self,
        calls: Arc<[SyscallId]>,
        phase: CatchPhase,
        condition: Option<Condition>,
    ) -> Catchpoint {
        let id = CatchpointId::new(self.next_catchpoint_id);
        self.next_catchpoint_id += 1;
        let catchpoint = Catchpoint {
            id,
            calls,
            phase,
            condition,
            hit_count: 0,
        };
        self.catchpoints.insert(id, catchpoint.clone());
        self.publish_catchpoints_changed();
        catchpoint
    }

    pub(super) fn remove_catchpoint(&mut self, id: CatchpointId) -> Result<Catchpoint> {
        let removed = self
            .catchpoints
            .remove(&id)
            .ok_or(Error::CatchpointNotFound(id.get()))?;
        self.publish_catchpoints_changed();
        Ok(removed)
    }

    fn publish_catchpoints_changed(&mut self) {
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::CatchpointsChanged {
            revision: self.revision,
        });
    }

    /// A thread stopped at a system call's entry or exit. It stops every
    /// thread when a catchpoint catches the call and its condition holds,
    /// or resumes as it would have.
    pub(super) fn handle_syscall_stop(&mut self, pid: Pid) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.thread_mut(pid)?.state = NativeThreadState::Stopped;
        let info = match self.ptrace.syscall_info(pid) {
            Ok(info) => info,
            // SIGKILL took it out of its stop; its exit follows.
            Err(nix::errno::Errno::ESRCH) => {
                let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
                inferior.thread_mut(pid)?.state = NativeThreadState::Running;
                return Ok(());
            }
            Err(error) => {
                return self.begin_visible_stop(
                    pid,
                    StopReason::Unclassifiable {
                        description: format!(
                            "a system-call stop whose call could not be read: {error}"
                        )
                        .into(),
                    },
                );
            }
        };
        let exit = match info.op {
            OP_ENTRY => false,
            OP_EXIT => true,
            op => {
                return self.begin_visible_stop(
                    pid,
                    StopReason::Unclassifiable {
                        description: format!("a system-call stop of unknown kind {op}").into(),
                    },
                );
            }
        };
        let entered = self.stopped_call(pid, &info, exit)?;
        let (call, args) = (entered.call, entered.args);
        let caught: Vec<CatchpointId> = self
            .catchpoints
            .values()
            .filter(|catchpoint| catchpoint.catches(call, exit))
            .map(|catchpoint| catchpoint.id)
            .collect();
        if caught.is_empty() {
            return self.restart_after_internal(pid);
        }
        let returned = exit.then_some(info.value);
        let mut captured = entered.captured;
        if exit {
            captured.extend(self.capture(pid, call, &args, ReadAt::Exit, returned));
            captured.sort_by_key(|capture| capture.arg);
        }
        let path = path_of(call, &captured);
        let captured: Arc<[Capture]> = captured.into();
        let stop = |catchpoints: Arc<[CatchpointId]>| {
            StopReason::Syscall(Box::new(SyscallStop {
                call,
                args,
                returned,
                path: path.clone(),
                captured: Arc::clone(&captured),
                catchpoints,
            }))
        };
        let mut stopping = Vec::new();
        for id in caught {
            let Some(catchpoint) = self.catchpoints.get_mut(&id) else {
                continue;
            };
            catchpoint.hit_count += 1;
            let condition = catchpoint.condition.clone();
            // A condition that cannot be judged stops, as a breakpoint's
            // does.
            if let Some(condition) = condition
                && self.judge_condition(
                    pid,
                    &condition,
                    &stop(Arc::from([id])),
                    ConditionOwner::Catchpoint(id),
                ) == Some(false)
            {
                continue;
            }
            stopping.push(id);
        }
        if stopping.is_empty() {
            return self.restart_after_internal(pid);
        }
        self.begin_visible_stop(pid, stop(stopping.into()))
    }

    /// The call a thread stopped in, its arguments, and what was read at
    /// its entry: at an entry, as the kernel reports them, and at an exit,
    /// as the thread's entry stop did, or as its registers hold them if the
    /// debugger missed the entry, as when the thread was in a call when
    /// catching began.
    fn stopped_call(&mut self, pid: Pid, info: &info::SyscallInfo, exit: bool) -> Result<Entered> {
        if !exit {
            let call = info::call_id(info);
            // Only a caught call's memory is read.
            let captured =
                if self.catchpoints.values().any(|catchpoint| {
                    catchpoint.catches(call, false) || catchpoint.catches(call, true)
                }) {
                    self.capture(pid, call, &info.args, ReadAt::Entry, None)
                } else {
                    Vec::new()
                };
            let entered = Entered {
                call,
                args: info.args,
                captured,
            };
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            inferior.thread_mut(pid)?.syscall = Some(entered.clone());
            return Ok(entered);
        }
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        if let Some(entered) = inferior.thread_mut(pid)?.syscall.take() {
            return Ok(entered);
        }
        let registers = self.ptrace.registers(pid)?;
        let (call, args) = info::from_registers(info.arch, &registers);
        Ok(Entered {
            call,
            args,
            captured: Vec::new(),
        })
    }

    /// Reads a call's pointer arguments at `phase` as the syscall tracer
    /// does, with its default limits.
    fn capture(
        &self,
        pid: Pid,
        call: SyscallId,
        args: &[u64; 6],
        phase: ReadAt,
        returned: Option<i64>,
    ) -> Vec<Capture> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Vec::new();
        };
        let mut memory = StopMemory {
            ptrace: &self.ptrace,
            pid,
            breakpoints: &inferior.breakpoints,
        };
        captures::capture(
            call,
            args,
            phase,
            returned.map(Outcome::of_return),
            Limits::default(),
            false,
            &mut memory,
        )
    }
}

/// A thread's call as its entry stop saw it.
#[derive(Debug, Clone)]
pub(super) struct Entered {
    call: SyscallId,
    args: [u64; 6],
    captured: Vec<Capture>,
}

/// A stopped thread's memory, with breakpoint traps hidden.
struct StopMemory<'a, P> {
    ptrace: &'a P,
    pid: Pid,
    breakpoints: &'a BTreeMap<VirtualAddress, BreakpointSite>,
}

impl<P: LinuxTraceOps> captures::Memory for StopMemory<'_, P> {
    fn read(&mut self, address: u64, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = read_logical_memory(
            self.ptrace,
            self.pid,
            self.breakpoints,
            VirtualAddress::new(address),
            buffer.len(),
        )
        .map_err(std::io::Error::other)?;
        buffer[..read.bytes.len()].copy_from_slice(&read.bytes);
        Ok(read.bytes.len())
    }
}

/// The path a call names, from what was read of its path argument.
fn path_of(call: SyscallId, captured: &[Capture]) -> Option<Arc<str>> {
    let spec = specs::spec(call)?;
    let index = spec.args.iter().position(|arg| matches!(arg, Arg::Path))?;
    let capture = captured
        .iter()
        .find(|capture| usize::from(capture.arg) == index)?;
    if !matches!(capture.state, CaptureState::Complete) {
        return None;
    }
    let bytes = capture.bytes.split(|&byte| byte == 0).next()?;
    Some(String::from_utf8_lossy(bytes).into())
}
