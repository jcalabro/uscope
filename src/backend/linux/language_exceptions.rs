//! Exceptions that language runtimes report: a breakpoint at each runtime
//! function that reports one the user wants to stop at, and the runtime's
//! own account of the exception when a thread enters it.

use std::collections::BTreeMap;
use std::sync::Arc;

use nix::unistd::Pid;

use crate::protocol::{ExceptionStops, LanguageException, StopReason};
use crate::runtime_model::RuntimeHook;
use crate::{Error, LanguageExceptionKind, Result, RuntimeId, VirtualAddress};

use super::breakpoints::remove_breakpoint_owner_from;
use super::frames::StackRoot;
use super::native::LinuxTraceOps;
use super::registers::x86_64_registers;
use super::{BreakpointOwner, Controller, debug_thread_id};

/// A runtime function whose entry stops for an exception.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HookSite {
    runtime: RuntimeId,
    hook: RuntimeHook,
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Changes which exceptions stop, and returns the previous choice.
    pub(super) fn set_exception_stops(&mut self, stops: ExceptionStops) -> Result<ExceptionStops> {
        let previous = std::mem::replace(&mut self.exception_stops, stops);
        if self.sites_live() {
            self.sync_runtime_hooks()?;
        }
        Ok(previous)
    }

    /// Installs a breakpoint at each loaded runtime's hook for an exception
    /// that stops, and removes the rest. The runtimes' signals take their
    /// defaults from the same runtimes.
    pub(super) fn sync_runtime_hooks(&mut self) -> Result<()> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(());
        };
        let runtimes = self.runtimes(inferior);
        self.signals.set_runtime_handled(
            runtimes
                .iter()
                .flat_map(|runtime| runtime.model.signals().handled.iter().copied()),
        );
        let stops = self.exception_stops;
        let wanted = runtimes
            .iter()
            .flat_map(|runtime| {
                runtime
                    .model
                    .hooks()
                    .iter()
                    .filter(|hook| stops.stops(hook.filter.id))
                    .filter_map(|hook| {
                        let address = runtime.module.virtual_address(hook.address).ok()?;
                        Some((
                            address,
                            HookSite {
                                runtime: runtime.id,
                                hook: *hook,
                            },
                        ))
                    })
            })
            .collect::<BTreeMap<_, _>>();
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let stale = inferior
            .runtime_hooks
            .iter()
            .filter(|(address, site)| wanted.get(address) != Some(site))
            .map(|(address, _)| *address)
            .collect::<Vec<_>>();
        for address in stale {
            inferior.runtime_hooks.remove(&address);
            remove_breakpoint_owner_from(
                &self.ptrace,
                inferior,
                address,
                BreakpointOwner::Runtime,
            )?;
        }
        let pid = inferior.memory_thread();
        for (address, site) in wanted {
            if inferior.runtime_hooks.contains_key(&address) {
                continue;
            }
            self.ptrace.install_breakpoint(
                pid,
                &mut inferior.breakpoints,
                address,
                BreakpointOwner::Runtime,
            )?;
            inferior.runtime_hooks.insert(address, site);
        }
        Ok(())
    }

    /// The exception a thread at `address` reports, when a runtime hook
    /// that stops is there. A report the runtime's memory cannot give is
    /// still a stop, which says why its message is missing.
    pub(super) fn runtime_exception(
        &self,
        pid: Pid,
        address: VirtualAddress,
    ) -> Option<StopReason> {
        let inferior = self.inferior.as_ref()?;
        let site = *inferior.runtime_hooks.get(&address)?;
        let kind = site.hook.filter.kind;
        if !self.exception_stops.stops(site.hook.filter.id) {
            return None;
        }
        let report = (|| {
            let runtime = self
                .runtimes(inferior)
                .into_iter()
                .find(|runtime| runtime.id == site.runtime)
                .ok_or_else(|| Arc::<str>::from("the runtime is no longer loaded"))?;
            let registers = self
                .ptrace
                .registers(pid)
                .map_err(|error| Arc::<str>::from(error.to_string()))?;
            let registers = x86_64_registers(&registers);
            self.with_runtime_stop(inferior, &runtime, pid, |stop| {
                runtime.model.exception(stop, site.hook.address, &registers)
            })
        })();
        let (message, value) = match report {
            Ok(report) => (report.message, report.value),
            Err(reason) => (
                format!("{}; its message is unreadable: {reason}", describe(kind)).into(),
                None,
            ),
        };
        Some(StopReason::LanguageException(LanguageException {
            kind,
            message,
            value,
        }))
    }

    /// Selects the frame a runtime's exception blames, the first one the
    /// program wrote, below the runtime's own frames that report it.
    pub(super) fn select_blamed_frame(&mut self, pid: Pid) {
        let Some(stop) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.public_stop.as_ref())
        else {
            return;
        };
        let (stop_id, context) = (stop.id, stop.selected);
        let Ok(trace) = self.backtrace(stop_id, &StackRoot::of_thread(pid)) else {
            return;
        };
        let Some(frame) = trace.user_frame().map(|frame| frame.id) else {
            return;
        };
        debug_assert_eq!(
            context,
            crate::ExecutionContext::Thread(debug_thread_id(pid))
        );
        if let Some(stop) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.public_stop.as_mut())
        {
            stop.selected_frames.insert(context, frame);
        }
    }
}

const fn describe(kind: LanguageExceptionKind) -> &'static str {
    match kind {
        LanguageExceptionKind::Raised => "an exception was raised",
        LanguageExceptionKind::Unhandled => "an exception was not handled",
        LanguageExceptionKind::Fatal => "the runtime failed",
    }
}
