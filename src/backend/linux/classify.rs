//! Classification of raw native stops into the events run control handles.

use std::collections::BTreeSet;

use nix::errno::Errno;
use nix::libc;
use nix::sys::signal::Signal as NixSignal;
use nix::unistd::Pid;

use crate::VirtualAddress;
use crate::backend::linux::debug_registers;
use crate::protocol::{StopReason, WatchpointId};

use super::native::LinuxTraceOps;
use super::{
    ClassifiedStop, Controller, ExpectedStop, NativeThreadState, PendingSignal, RawStopRecord,
    SignalMetadata, TRAP_HARDWARE_BREAKPOINT, TRAP_UNKNOWN,
};

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn classify_stop(&self, pid: Pid, signal: NixSignal) -> ClassifiedStop {
        let status = format!("Stopped({pid}, {signal})");
        let siginfo = self.ptrace.signal_metadata(pid);
        let expected = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.threads.get(&pid))
            .map_or(ExpectedStop::None, |thread| thread.expected.clone());
        let starting = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.threads.get(&pid))
            .is_some_and(|thread| matches!(thread.state, NativeThreadState::Starting));
        let debugger_requested = signal == NixSignal::SIGSTOP
            && self
                .inferior
                .as_ref()
                .and_then(|inferior| inferior.threads.get(&pid))
                .is_some_and(|thread| thread.debugger_stop_pending)
            && siginfo.as_ref().is_ok_and(|metadata| {
                metadata.code == libc::SI_TKILL
                    && metadata.sender
                        == Some(i32::try_from(std::process::id()).unwrap_or(i32::MAX))
            });
        let expected_trace = signal == NixSignal::SIGTRAP
            && siginfo.as_ref().is_ok_and(is_single_step_trap)
            && matches!(
                expected,
                ExpectedStop::BreakpointRepair { .. } | ExpectedStop::UserStep { .. }
            );
        let code = siginfo.as_ref().ok().map(|metadata| metadata.code);
        let watch = if signal == NixSignal::SIGTRAP {
            self.watch_status(pid, code)
        } else {
            WatchStatus::Absent
        };
        // Only an int3 reports SI_KERNEL. Watchpoint traps and SIGTRAPs sent
        // by a process also stop after an instruction, and rewinding the PC
        // for them would execute that instruction twice.
        let breakpoint =
            (signal == NixSignal::SIGTRAP && !expected_trace && code == Some(libc::SI_KERNEL))
                .then(|| self.normalize_breakpoint_pc(pid))
                .flatten();

        classify_stop_evidence(
            signal,
            status,
            siginfo,
            &expected,
            starting,
            debugger_requested,
            breakpoint,
            watch,
        )
    }

    /// Reads and consumes DR6 for stops raised by a debug exception.
    ///
    /// The kernel resets its virtual DR6 only on the next debug exception, so
    /// at an int3, signal, or syscall-step stop it still describes an earlier
    /// hit. Only hardware-breakpoint traps and single steps are consulted, and
    /// the status is cleared once read.
    pub(super) fn watch_status(&self, pid: Pid, code: Option<i32>) -> WatchStatus {
        let Some(inferior) = self.inferior.as_ref() else {
            return WatchStatus::Absent;
        };
        let consult = code == Some(TRAP_HARDWARE_BREAKPOINT)
            || (code == Some(libc::TRAP_TRACE) && !inferior.watch.plan.is_empty());
        if !consult {
            return WatchStatus::Absent;
        }
        let status = match self
            .ptrace
            .read_debug_register(pid, debug_registers::STATUS_REGISTER)
        {
            Ok(status) => status,
            Err(error) => return WatchStatus::Unknown(format!("reading DR6 failed: {error}")),
        };
        let _ = self.ptrace.write_debug_register(
            pid,
            debug_registers::STATUS_REGISTER,
            debug_registers::STATUS_IDLE,
        );
        if !debug_registers::status_has_hits(status) {
            return WatchStatus::Absent;
        }
        match inferior.watch.plan.owners_for_status(status) {
            Ok(owners) => WatchStatus::Hits(owners),
            Err(unknown) => WatchStatus::Unknown(format!(
                "DR6 {:#x} reports a hit in a slot no watchpoint owns",
                unknown.status
            )),
        }
    }

    pub(super) fn normalize_breakpoint_pc(&self, pid: Pid) -> Option<VirtualAddress> {
        let mut registers = self.ptrace.registers(pid).ok()?;
        let address = VirtualAddress::new(registers.rip.checked_sub(1)?);
        let installed = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.breakpoints.get(&address))
            .is_some_and(|site| site.installed);
        if !installed {
            return None;
        }

        registers.rip = address.get();
        self.ptrace.set_registers(pid, registers).ok()?;
        Some(address)
    }
}

/// Chooses the one primary reason published for coincident all-stop events.
///
/// Lower-priority reasons remain attached to their native threads, including
/// pending signals. Control completions must outrank exceptions so resuming an
/// unrelated signal stop cannot silently repair and consume a user breakpoint
/// or completed step. Unsafe state transitions outrank ordinary control stops.
pub(super) const fn visible_stop_priority(reason: &StopReason) -> u8 {
    match reason {
        StopReason::Attach | StopReason::Pause => 0,
        StopReason::Exception(_) => 1,
        StopReason::Breakpoint { .. }
        | StopReason::Watchpoint { .. }
        | StopReason::WatchpointInvalidated { .. }
        | StopReason::WatchpointArmFailed { .. }
        | StopReason::Step { .. }
        | StopReason::ThreadExited { .. } => 2,
        StopReason::Exec => 3,
        StopReason::Unclassifiable { .. } => 4,
        StopReason::Exited(_) | StopReason::CoreDump { .. } => 5,
    }
}

pub(super) const fn is_stopping_signal(signal: NixSignal) -> bool {
    matches!(
        signal,
        NixSignal::SIGSTOP | NixSignal::SIGTSTP | NixSignal::SIGTTIN | NixSignal::SIGTTOU
    )
}

/// DR6 evidence for one SIGTRAP stop.
#[derive(Debug)]
pub(super) enum WatchStatus {
    /// No debug exception reported a watchpoint slot.
    Absent,
    /// These watchpoints own the reported slots.
    Hits(BTreeSet<WatchpointId>),
    /// The status could not be read or names an unowned slot.
    Unknown(String),
}

#[expect(
    clippy::too_many_arguments,
    reason = "the pure classifier receives every piece of native stop evidence explicitly"
)]
pub(super) fn classify_stop_evidence(
    signal: NixSignal,
    status: String,
    siginfo: std::result::Result<SignalMetadata, Errno>,
    expected: &ExpectedStop,
    starting: bool,
    debugger_requested: bool,
    breakpoint: Option<VirtualAddress>,
    watch: WatchStatus,
) -> ClassifiedStop {
    if signal == NixSignal::SIGSTOP && starting {
        return ClassifiedStop::ThreadStart;
    }
    if debugger_requested {
        return ClassifiedStop::DebuggerRequested;
    }
    if signal == NixSignal::SIGTRAP
        && siginfo
            .as_ref()
            .is_ok_and(|metadata| metadata.code == TRAP_HARDWARE_BREAKPOINT)
    {
        return match watch {
            WatchStatus::Hits(owners) if !owners.is_empty() => ClassifiedStop::Watch(owners),
            WatchStatus::Unknown(description) => ClassifiedStop::Unclassifiable(RawStopRecord {
                status: format!("{status}; {description}"),
                siginfo,
            }),
            WatchStatus::Hits(_) | WatchStatus::Absent => {
                ClassifiedStop::Unclassifiable(RawStopRecord {
                    status: format!("{status}; hardware breakpoint trap without a reported slot"),
                    siginfo,
                })
            }
        };
    }
    if signal == NixSignal::SIGTRAP
        && siginfo.as_ref().is_ok_and(is_single_step_trap)
        && matches!(
            expected,
            ExpectedStop::BreakpointRepair { .. } | ExpectedStop::UserStep { .. }
        )
    {
        return match watch {
            WatchStatus::Unknown(description) => ClassifiedStop::Unclassifiable(RawStopRecord {
                status: format!("{status}; {description}"),
                siginfo,
            }),
            WatchStatus::Hits(watch) => ClassifiedStop::Trace { watch },
            WatchStatus::Absent => ClassifiedStop::Trace {
                watch: BTreeSet::new(),
            },
        };
    }
    if let Some(address) = breakpoint {
        return ClassifiedStop::Breakpoint(address);
    }

    match siginfo {
        Ok(metadata) if signal != NixSignal::SIGTRAP || metadata.code <= 0 => {
            ClassifiedStop::SignalDelivery(PendingSignal {
                signal,
                code: metadata.code,
                sender: metadata.sender,
            })
        }
        Err(Errno::EINVAL) if is_stopping_signal(signal) => ClassifiedStop::GroupStop(signal),
        outcome => ClassifiedStop::Unclassifiable(RawStopRecord {
            status,
            siginfo: outcome,
        }),
    }
}

/// Returns whether a SIGTRAP reports a completed single step. x86 Linux
/// reports a step across a `syscall` instruction from the system call's exit
/// path as `TRAP_BRKPT`; an `int3` reports `SI_KERNEL` instead.
pub(super) const fn is_single_step_trap(metadata: &SignalMetadata) -> bool {
    matches!(
        metadata.code,
        libc::TRAP_TRACE | libc::TRAP_BRKPT | TRAP_UNKNOWN
    )
}

pub(super) fn format_raw_stop(raw: &RawStopRecord) -> String {
    match raw.siginfo {
        Ok(metadata) => format!(
            "{}; siginfo code={} sender={:?}",
            raw.status, metadata.code, metadata.sender
        ),
        Err(error) => format!("{}; PTRACE_GETSIGINFO failed: {error}", raw.status),
    }
}
