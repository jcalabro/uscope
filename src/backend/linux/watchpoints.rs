//! Hardware watchpoints: arming debug registers, attributing hits, and
//! invalidating watchpoints whose storage's lifetime ended.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::signals::Signal;
use nix::errno::Errno;
use nix::unistd::Pid;

use crate::backend::linux::debug_registers;
use crate::debug_info::StorageClass;
use crate::debug_info::VariableRuntimeError;
use crate::protocol::{
    DebuggerEvent, FrameScopeEvidence, InvalidatedWatchpoint, StopReason, WatchAccess, WatchScope,
    Watchpoint, WatchpointHit, WatchpointId, WatchpointInvalidation, WatchpointSpec,
};
use crate::unwind::{CallerProvider, CallerResult, DEFAULT_MAX_FRAMES, FrameContext};
use crate::{
    AddressRange, Error, ImageAddress, InspectedValue, MemoryReadCompletion, Result, StackFrameId,
    UnwindTermination, VariableValueSource, VirtualAddress,
};

use super::debug_registers::{DebugRegisterPlan, SlotAccess};
use super::frames::{DwarfCallerProvider, frame_lookup_address};
use super::memory::{PtraceMemory, read_logical_memory};
use super::native::{InspectionOps, LinuxTraceOps, is_vanished_tracee};
use super::registers::x86_64_registers;
use super::stepping::x86_64_activation_has_returned;
use super::{
    Controller, Inferior, LinuxError, NativeThreadState, WatchRecord, backend_error, debug_pid,
    debug_thread_id, validate_image_current,
};

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn add_watchpoint(
        &mut self,
        spec: WatchpointSpec,
        access: WatchAccess,
    ) -> Result<Watchpoint> {
        let slot_access = match access {
            // A change is judged after the store that the hardware traps.
            WatchAccess::Change | WatchAccess::Write => SlotAccess::Write,
            WatchAccess::ReadWrite => SlotAccess::ReadWrite,
            WatchAccess::Read => return Err(Error::UnsupportedWatchAccess(access)),
        };
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        if !self.sites_live() {
            return Err(Error::NotStopped);
        }
        validate_image_current(inferior)?;
        let (expression, address, byte_size, type_info, scope, frame) = match spec {
            WatchpointSpec::Target(target) => {
                let target = *target;
                // A target is armed only at the stop that resolved it, not
                // at a later stop or while the inferior runs.
                if inferior.public_stop.as_ref().map(|stop| stop.id) != Some(target.stop_id) {
                    return Err(Error::StaleStop);
                }
                (
                    Some(target.expression),
                    target.address,
                    target.byte_size,
                    target.type_info,
                    target.scope,
                    target.frame,
                )
            }
            WatchpointSpec::Location { address, byte_size } => {
                (None, address, byte_size, None, WatchScope::Location, None)
            }
        };
        let chunks = debug_registers::split_range(address.get(), byte_size).map_err(|error| {
            watch_range_error(address, byte_size, error, inferior.watch.plan.free_slots())
        })?;
        let id = WatchpointId::new(self.next_watchpoint_id);
        let next_id = self
            .next_watchpoint_id
            .checked_add(1)
            .ok_or_else(|| backend_error(LinuxError::WatchpointIdExhausted))?;
        let plan = inferior
            .watch
            .plan
            .with_watchpoint(id, &chunks, slot_access)
            .map_err(|error| Error::WatchpointCapacity {
                required: u64::try_from(error.required).expect("slot count fits u64"),
                available: u64::try_from(error.available).expect("slot count fits u64"),
            })?;
        // Read before arming, so a failed read leaves nothing armed.
        let observed = self.read_watched_bytes(address, byte_size)?;
        self.arm_all_threads(plan)?;

        let watchpoint = Watchpoint {
            id,
            access,
            expression,
            address,
            byte_size,
            type_info,
            scope,
            coverage: chunks
                .iter()
                .map(|chunk| AddressRange {
                    start: VirtualAddress::new(chunk.address),
                    end: VirtualAddress::new(chunk.end()),
                })
                .collect::<Vec<_>>()
                .into(),
        };
        self.inferior
            .as_mut()
            .expect("armed inferior exists")
            .watch
            .watchpoints
            .insert(
                id,
                WatchRecord {
                    watchpoint: watchpoint.clone(),
                    frame,
                    observed,
                },
            );
        self.next_watchpoint_id = next_id;
        self.publish_watchpoints_changed();
        Ok(watchpoint)
    }

    pub(super) fn remove_watchpoint(&mut self, id: WatchpointId) -> Result<Watchpoint> {
        let inferior = self
            .inferior
            .as_ref()
            .filter(|inferior| inferior.watch.watchpoints.contains_key(&id))
            .ok_or(Error::WatchpointNotFound(id.get()))?;
        let plan = inferior.watch.plan.without_watchpoint(id);
        self.arm_all_threads(plan)?;
        let record = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.watch.watchpoints.remove(&id))
            .expect("removed watchpoint was recorded");
        self.publish_watchpoints_changed();
        Ok(record.watchpoint)
    }

    pub(super) fn remove_all_watchpoints(&mut self) -> Result<Arc<[Watchpoint]>> {
        if self
            .inferior
            .as_ref()
            .is_none_or(|inferior| inferior.watch.watchpoints.is_empty())
        {
            return Ok(Arc::from([]));
        }
        self.arm_all_threads(DebugRegisterPlan::default())?;
        let removed = std::mem::take(
            &mut self
                .inferior
                .as_mut()
                .expect("disarmed inferior exists")
                .watch
                .watchpoints,
        );
        self.publish_watchpoints_changed();
        Ok(removed
            .into_values()
            .map(|record| record.watchpoint)
            .collect::<Vec<_>>()
            .into())
    }

    pub(super) fn publish_watchpoints_changed(&mut self) {
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::WatchpointsChanged {
            revision: self.revision,
        });
    }

    /// Installs `plan` on every stopped thread or on none of them.
    ///
    /// A thread that has already begun exiting is skipped; its exit is
    /// processed normally. So is one that does not carry the current plan,
    /// its arming having failed: it cannot run until it is armed before a
    /// resume. Rolling a thread back therefore only rewrites slots the plan
    /// it previously carried already reserved, so rollback needs no new
    /// kernel capacity.
    pub(super) fn arm_all_threads(&mut self, plan: DebugRegisterPlan) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let previous = inferior.watch.plan.clone();
        let generation = inferior.watch.generation;
        let threads = inferior
            .threads
            .iter()
            .filter_map(|(&pid, thread)| {
                (matches!(thread.state, NativeThreadState::Stopped)
                    && thread.armed == Some(generation))
                .then_some(pid)
            })
            .collect::<Vec<_>>();
        let mut programmed = Vec::with_capacity(threads.len());
        for pid in threads {
            match program_debug_registers(&self.ptrace, pid, &plan) {
                Ok(()) => programmed.push(pid),
                Err(ArmFailure::ThreadGone) => {}
                Err(failure) => {
                    let cause = arm_error(pid, &failure);
                    // The failing thread may have been partly rewritten, so it
                    // is restored along with every thread already armed.
                    for &armed in std::iter::once(&pid).chain(programmed.iter().rev()) {
                        match program_debug_registers(&self.ptrace, armed, &previous) {
                            Ok(()) | Err(ArmFailure::ThreadGone) => {}
                            Err(recovery) => {
                                let _ = self.ptrace.kill(inferior.tgid, Signal::SIGKILL);
                                return Err(backend_error(LinuxError::WatchpointArmRecovery {
                                    cause: cause.to_string(),
                                    recovery: arm_error(armed, &recovery).to_string(),
                                }));
                            }
                        }
                    }
                    return Err(cause);
                }
            }
        }
        inferior.watch.generation = inferior.watch.generation.wrapping_add(1);
        inferior.watch.plan = plan;
        let generation = inferior.watch.generation;
        for pid in programmed {
            inferior.thread_mut(pid)?.armed = Some(generation);
        }
        Ok(())
    }

    /// Programs any stopped thread whose registers do not carry the current
    /// plan, so no thread ever runs unwatched. Failure leaves the inferior
    /// stopped.
    pub(super) fn sync_debug_registers(&mut self) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let generation = inferior.watch.generation;
        let stale = inferior
            .threads
            .iter()
            .filter(|(_, thread)| {
                matches!(thread.state, NativeThreadState::Stopped)
                    && thread.armed != Some(generation)
            })
            .map(|(&pid, _)| pid)
            .collect::<Vec<_>>();
        for pid in stale {
            match program_debug_registers(&self.ptrace, pid, &inferior.watch.plan) {
                Ok(()) => {
                    inferior.thread_mut(pid)?.armed = Some(generation);
                }
                Err(ArmFailure::ThreadGone) => {}
                Err(failure) => return Err(arm_error(pid, &failure)),
            }
        }
        Ok(())
    }

    /// Arms a newly started thread before it executes. Returns the failure
    /// to publish when it cannot be armed.
    pub(super) fn arm_new_thread(&mut self, pid: Pid) -> Option<Error> {
        let inferior = self.inferior.as_mut()?;
        let generation = inferior.watch.generation;
        // Debug registers are never inherited, so an unarmed thread already
        // matches an empty plan.
        let result = if inferior.watch.plan.is_empty() {
            Ok(())
        } else {
            program_debug_registers(&self.ptrace, pid, &inferior.watch.plan)
        };
        match result {
            Ok(()) => {
                inferior.threads.get_mut(&pid)?.armed = Some(generation);
                None
            }
            Err(ArmFailure::ThreadGone) => None,
            Err(failure) => Some(arm_error(pid, &failure)),
        }
    }

    /// Clears debug-register state a previous tracer may have left armed in
    /// an attached process, which would otherwise kill it with SIGTRAP.
    pub(super) fn clear_attached_debug_registers(&mut self) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let generation = inferior.watch.generation;
        let empty = DebugRegisterPlan::default();
        for (&pid, thread) in &mut inferior.threads {
            match program_debug_registers(&self.ptrace, pid, &empty) {
                Ok(()) => thread.armed = Some(generation),
                Err(ArmFailure::ThreadGone) => {}
                Err(failure) => return Err(arm_error(pid, &failure)),
            }
        }
        Ok(())
    }

    /// Disarms every thread before the process leaves debugger control. The
    /// kernel keeps debug registers armed across `PTRACE_DETACH`, and an
    /// untraced hit would kill the process with SIGTRAP. Every thread is
    /// attempted even if one fails.
    pub(super) fn disarm_for_detach(&mut self) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let empty = DebugRegisterPlan::default();
        let mut first_error = None;
        for (&pid, thread) in &inferior.threads {
            if thread.armed.is_none() && inferior.watch.plan.is_empty() {
                continue;
            }
            match program_debug_registers(&self.ptrace, pid, &empty) {
                Ok(()) | Err(ArmFailure::ThreadGone) => {}
                Err(failure) => {
                    first_error.get_or_insert_with(|| arm_error(pid, &failure));
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Records the watched bytes just before execution resumes so the next
    /// hit reports what the access changed since the debugger last looked,
    /// including the debugger's own writes.
    pub(super) fn refresh_watch_baselines(&mut self) -> Result<()> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(());
        };
        let observed = inferior
            .watch
            .watchpoints
            .iter()
            .map(|(&id, record)| {
                self.read_watched_bytes(record.watchpoint.address, record.watchpoint.byte_size)
                    .map(|bytes| (id, bytes))
            })
            .collect::<Result<Vec<_>>>()?;
        let inferior = self.inferior.as_mut().expect("inferior exists");
        for (id, bytes) in observed {
            inferior
                .watch
                .watchpoints
                .get_mut(&id)
                .expect("watchpoint exists")
                .observed = bytes;
        }
        Ok(())
    }

    /// Resolves the pending watch evidence of every stopped thread once the
    /// whole process is stopped: invalidates scoped watchpoints whose storage
    /// ended, reports hits on the remaining ones, and publishes the result
    /// as each thread's stop reason.
    pub(super) fn evaluate_watchpoints(&mut self) -> Result<()> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(());
        };
        if inferior.watch.watchpoints.is_empty()
            && inferior
                .threads
                .values()
                .all(|thread| thread.watch_hits.is_empty())
        {
            return Ok(());
        }

        let mut invalid = BTreeMap::new();
        for (&id, record) in &inferior.watch.watchpoints {
            if let Some(reason) = self.watch_invalidation(inferior, record)? {
                invalid.insert(id, reason);
            }
        }
        let hit = inferior
            .threads
            .values()
            .flat_map(|thread| thread.watch_hits.iter().copied())
            .filter(|id| !invalid.contains_key(id))
            .collect::<BTreeSet<_>>();
        let current = hit
            .iter()
            .filter_map(|id| {
                inferior
                    .watch
                    .watchpoints
                    .get(id)
                    .map(|record| (*id, record))
            })
            .map(|(id, record)| {
                self.read_watched_bytes(record.watchpoint.address, record.watchpoint.byte_size)
                    .map(|bytes| (id, bytes))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let invalidated = invalid
            .iter()
            .map(|(id, reason)| InvalidatedWatchpoint {
                watchpoint: inferior.watch.watchpoints[id].watchpoint.clone(),
                reason: *reason,
            })
            .collect::<Vec<_>>();

        let inferior = self.inferior.as_mut().expect("inferior exists");
        for (&pid, thread) in &mut inferior.threads {
            let owners = std::mem::take(&mut thread.watch_hits);
            if owners.is_empty() {
                continue;
            }
            let hits = owners
                .iter()
                .filter_map(|id| {
                    let record = inferior.watch.watchpoints.get(id)?;
                    current.get(id).map(|bytes| WatchpointHit {
                        watchpoint: *id,
                        thread: debug_thread_id(pid),
                        previous: record.observed.clone(),
                        current: bytes.clone(),
                    })
                })
                .collect::<Vec<_>>();
            let reason = if hits.is_empty() {
                StopReason::WatchpointInvalidated {
                    invalidated: invalidated
                        .iter()
                        .filter(|entry| owners.contains(&entry.watchpoint.id))
                        .cloned()
                        .collect::<Vec<_>>()
                        .into(),
                }
            } else {
                StopReason::Watchpoint { hits: hits.into() }
            };
            if let Some(barrier) = inferior.barrier.as_mut()
                && barrier.triggering_thread == pid
                && matches!(barrier.reason, Some(StopReason::Watchpoint { .. }))
            {
                barrier.reason = Some(reason.clone());
            }
            thread.reason = Some(reason);
        }
        for (id, bytes) in current {
            if let Some(record) = inferior.watch.watchpoints.get_mut(&id) {
                record.observed = bytes;
            }
        }
        self.remove_invalidated_watchpoints(invalidated)
    }

    /// Disarms and publishes watchpoints whose storage's lifetime ended.
    pub(super) fn remove_invalidated_watchpoints(
        &mut self,
        invalidated: Vec<InvalidatedWatchpoint>,
    ) -> Result<()> {
        if invalidated.is_empty() {
            return Ok(());
        }
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let plan = invalidated
            .iter()
            .fold(inferior.watch.plan.clone(), |plan, entry| {
                plan.without_watchpoint(entry.watchpoint.id)
            });
        self.arm_all_threads(plan)?;
        let inferior = self.inferior.as_mut().expect("inferior exists");
        for entry in &invalidated {
            inferior.watch.watchpoints.remove(&entry.watchpoint.id);
        }
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::WatchpointsInvalidated {
            revision: self.revision,
            invalidated: invalidated.into(),
        });
        self.publish_watchpoints_changed();
        Ok(())
    }

    /// Discards the watchpoints of a process that no longer exists or whose
    /// image was replaced.
    pub(super) fn discard_watchpoints(&mut self) {
        let Some(inferior) = self.inferior.as_mut() else {
            return;
        };
        let had_watchpoints = !inferior.watch.watchpoints.is_empty();
        inferior.watch.watchpoints.clear();
        inferior.watch.plan = DebugRegisterPlan::default();
        inferior.watch.generation = inferior.watch.generation.wrapping_add(1);
        if had_watchpoints {
            self.publish_watchpoints_changed();
        }
    }
}

impl<P: InspectionOps> Controller<P> {
    /// Reads the current watched bytes through a stopped thread, hiding
    /// software-breakpoint bytes. Unreadable memory is reported as `None`, as
    /// is memory no stopped thread can reach because each was killed out of
    /// its ptrace-stop, as a sibling's `exit_group` does. Any other failure
    /// is an error, never a value.
    pub(super) fn read_watched_bytes(
        &self,
        address: VirtualAddress,
        byte_size: u64,
    ) -> Result<Option<Arc<[u8]>>> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let size = usize::try_from(byte_size).map_err(|_| Error::AddressOverflow)?;
        let stopped = inferior
            .threads
            .iter()
            .filter(|(_, thread)| matches!(thread.state, NativeThreadState::Stopped));
        for (&pid, _) in stopped {
            match read_logical_memory(&self.ptrace, pid, &inferior.breakpoints, address, size) {
                Ok(read) => {
                    return Ok(matches!(read.completion, MemoryReadCompletion::Complete)
                        .then(|| read.bytes.into()));
                }
                Err(error) if is_vanished_tracee(&error) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }

    /// The change watchpoints among `owners` whose watched bytes equal those
    /// the debugger last observed, so the access changed nothing. Bytes that
    /// became unreadable, or readable, count as a change.
    pub(super) fn unchanged_watchpoints(
        &self,
        owners: impl IntoIterator<Item = WatchpointId>,
    ) -> Result<BTreeSet<WatchpointId>> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(BTreeSet::new());
        };
        let mut unchanged = BTreeSet::new();
        for id in owners {
            let Some(record) = inferior.watch.watchpoints.get(&id) else {
                continue;
            };
            if record.watchpoint.access == WatchAccess::Change
                && self
                    .read_watched_bytes(record.watchpoint.address, record.watchpoint.byte_size)?
                    == record.observed
            {
                unchanged.insert(id);
            }
        }
        Ok(unchanged)
    }

    /// The watchpoints among a thread's hit `owners` that report its
    /// access: all but the change watchpoints it left unchanged.
    pub(super) fn reportable_watch_hits(
        &self,
        mut owners: BTreeSet<WatchpointId>,
    ) -> Result<BTreeSet<WatchpointId>> {
        let unchanged = self.unchanged_watchpoints(owners.iter().copied())?;
        owners.retain(|id| !unchanged.contains(id));
        Ok(owners)
    }

    pub(super) fn watch_invalidation(
        &self,
        inferior: &Inferior,
        record: &WatchRecord,
    ) -> Result<Option<WatchpointInvalidation>> {
        Ok(match &record.watchpoint.scope {
            WatchScope::Location => None,
            WatchScope::Static { module } => (!self.modules.contains_key(module))
                .then_some(WatchpointInvalidation::ModuleUnloaded),
            WatchScope::ThreadLocal { thread } => (!debug_pid(*thread)
                .is_ok_and(|pid| inferior.threads.contains_key(&pid)))
            .then_some(WatchpointInvalidation::OwnerThreadExited),
            WatchScope::Frame { thread, activation } => {
                let pid = debug_pid(*thread)?;
                if inferior.threads.contains_key(&pid) {
                    let evidence = record
                        .frame
                        .as_ref()
                        .expect("frame-scoped watchpoints carry scope evidence");
                    (!self.frame_scope_is_live(inferior, pid, *activation, evidence)?)
                        .then_some(WatchpointInvalidation::ScopeExited)
                } else {
                    Some(WatchpointInvalidation::OwnerThreadExited)
                }
            }
        })
    }

    /// Whether the owner thread still executes the activation that declared
    /// a frame-scoped object, inside the object's lexical scope.
    ///
    /// The activation is found by unwinding the owner's stack to the frame
    /// whose canonical frame address matches. A frame at that address running
    /// a different function means a tail call replaced the activation; a
    /// frame outside the object's scope ranges means its block ended. When
    /// unwinding fails before reaching the activation, the stack pointer
    /// still proves a return on x86-64's downward-growing stack.
    pub(super) fn frame_scope_is_live(
        &self,
        inferior: &Inferior,
        pid: Pid,
        activation: VirtualAddress,
        evidence: &FrameScopeEvidence,
    ) -> Result<bool> {
        let Some(module) = self
            .modules
            .get(&evidence.module)
            .filter(|module| module.loaded.image == evidence.image)
        else {
            return Ok(false);
        };
        let native = self.ptrace.registers(pid)?;
        let mut provider = DwarfCallerProvider {
            modules: self.unwind_modules(inferior),
            registers: x86_64_registers(&native),
            memory: PtraceMemory {
                ptrace: &self.ptrace,
                pid,
            },
            first: true,
        };
        let mut context = FrameContext {
            instruction: VirtualAddress::new(native.rip),
            cfa: None,
            signal_frame: false,
        };
        let unproven = !x86_64_activation_has_returned(native.rsp, activation);
        for level in 0..DEFAULT_MAX_FRAMES {
            let caller = match provider.caller(&context) {
                CallerResult::Caller(caller) => caller,
                CallerResult::Finished(UnwindTermination::Complete) => return Ok(false),
                CallerResult::Finished(_) => return Ok(unproven),
            };
            if caller.cfa == Some(activation) {
                let level = u32::try_from(level).expect("frame limit fits u32");
                let Some(image_address) = frame_lookup_address(level, &context)
                    .and_then(|address| module.loaded.image_address(address).ok())
                    .filter(|address| module.image.contains_address(*address))
                else {
                    return Ok(false);
                };
                let location = module.image.locate(image_address);
                return Ok(location.physical_instance == Some(evidence.function)
                    && evidence
                        .ranges
                        .iter()
                        .any(|range| range.contains(image_address)));
            }
            if caller.cfa.is_some_and(|cfa| cfa > activation) {
                return Ok(false);
            }
            context = caller;
        }
        Ok(unproven)
    }

    /// The lifetime of a data object's storage, which a watch of memory
    /// inside it shares. `local` is the frame's address in the object's
    /// image when the object is a local or parameter of the frame.
    pub(super) fn root_watch_scope(
        &self,
        inferior: &Inferior,
        pid: Pid,
        frame: StackFrameId,
        module: crate::ModuleId,
        storage: crate::debug_info::ObjectStorage,
        local: Option<ImageAddress>,
    ) -> Result<(WatchScope, Option<FrameScopeEvidence>)> {
        match storage.class {
            StorageClass::Static => Ok((WatchScope::Static { module }, None)),
            StorageClass::ThreadLocal => Ok((
                WatchScope::ThreadLocal {
                    thread: debug_thread_id(pid),
                },
                None,
            )),
            StorageClass::Indirect => Ok((WatchScope::Location, None)),
            StorageClass::NotMemory | StorageClass::Frame { stable: false, .. } => {
                Err(Error::WatchTargetUnsupported(
                    "the object's location changes within its scope".into(),
                ))
            }
            StorageClass::Frame {
                moving_stack: true, ..
            } => Err(Error::WatchTargetUnsupported(
                "the language runtime may move this stack object; watch a heap or global object"
                    .into(),
            )),
            StorageClass::Frame { .. } => {
                let Some(address) = local else {
                    return Err(Error::WatchTargetUnsupported(
                        "a frame-relative global has no owning activation".into(),
                    ));
                };
                let image = &self
                    .modules
                    .get(&module)
                    .ok_or(Error::ModuleNotLoaded(module))?
                    .image;
                let activation =
                    self.resolve_frame(inferior, pid, frame)?
                        .cfa
                        .map_err(|error| {
                            let reason: Arc<str> = match error {
                                VariableRuntimeError::Unavailable(reason) => {
                                    reason.to_string().into()
                                }
                                VariableRuntimeError::Malformed(reason)
                                | VariableRuntimeError::Fatal(reason) => reason,
                            };
                            Error::WatchTargetUnsupported(
                                format!("the declaring activation is unavailable: {reason}").into(),
                            )
                        })?;
                let function = image.locate(address).physical_instance.ok_or_else(|| {
                    Error::WatchTargetUnsupported(
                        "no function describes the declaring activation".into(),
                    )
                })?;
                Ok((
                    WatchScope::Frame {
                        thread: debug_thread_id(pid),
                        activation,
                    },
                    Some(FrameScopeEvidence {
                        module,
                        image: image.id(),
                        function,
                        ranges: storage.ranges,
                    }),
                ))
            }
        }
    }
}

/// The memory a resolved value occupies, or why it cannot be watched.
pub(super) fn watchable_storage(value: &InspectedValue) -> Result<(VirtualAddress, u64)> {
    let source = match &value.state {
        crate::VariableState::Available { source, .. }
        | crate::VariableState::Invalid { source, .. } => source,
        crate::VariableState::Unavailable(reason) => {
            return Err(Error::WatchTargetUnavailable(reason.to_string().into()));
        }
        crate::VariableState::Malformed(reason) => {
            return Err(Error::WatchTargetUnavailable(Arc::clone(
                &reason.description,
            )));
        }
    };
    let address = match source {
        VariableValueSource::Memory(address) => *address,
        VariableValueSource::Register(register) => {
            return Err(Error::WatchTargetNotInMemory(
                format!("the value is held in register {}", register.name).into(),
            ));
        }
        VariableValueSource::Constant => {
            return Err(Error::WatchTargetNotInMemory(
                "the value is a debug-information constant".into(),
            ));
        }
        VariableValueSource::Computed => {
            return Err(Error::WatchTargetNotInMemory(
                "the value is computed, such as a bit-field or an optimized expression".into(),
            ));
        }
        VariableValueSource::ImplicitPointer => {
            return Err(Error::WatchTargetNotInMemory(
                "optimization eliminated the pointer's address".into(),
            ));
        }
    };
    let byte_size = value
        .type_info
        .as_ref()
        .and_then(|info| info.byte_size)
        .filter(|size| *size > 0)
        .ok_or_else(|| Error::WatchTargetUnsupported("the value's size is unknown".into()))?;
    Ok((address, byte_size))
}

pub(super) fn watch_range_error(
    address: VirtualAddress,
    byte_size: u64,
    error: debug_registers::RangeError,
    available: usize,
) -> Error {
    let reason = match error {
        debug_registers::RangeError::Empty => "a watchpoint must cover at least one byte",
        debug_registers::RangeError::Overflow => "the range wraps the address space",
        debug_registers::RangeError::OutsideUserSpace => {
            "the range reaches memory the kernel does not let user debuggers watch"
        }
        debug_registers::RangeError::TooLarge { required } => {
            return Error::WatchpointCapacity {
                required,
                available: u64::try_from(available).expect("slot count fits u64"),
            };
        }
    };
    Error::InvalidWatchRange {
        address,
        byte_size,
        reason: reason.into(),
    }
}

/// Why one thread's debug registers could not be programmed.
#[derive(Debug)]
pub(super) enum ArmFailure {
    /// The thread is exiting.
    ThreadGone,
    /// Other hardware-breakpoint users hold the thread's slots.
    Busy,
    /// The kernel refused a value or the readback disagreed, as in sandboxes
    /// that ignore debug-register writes.
    Unsupported(String),
    System(Errno),
}

pub(super) fn arm_failure(error: Errno) -> ArmFailure {
    match error {
        Errno::ESRCH => ArmFailure::ThreadGone,
        Errno::ENOSPC => ArmFailure::Busy,
        Errno::EINVAL | Errno::EIO => {
            ArmFailure::Unsupported(format!("the kernel refused a debug register: {error}"))
        }
        error => ArmFailure::System(error),
    }
}

pub(super) fn arm_error(pid: Pid, failure: &ArmFailure) -> Error {
    match failure {
        ArmFailure::ThreadGone => Error::NotRunning,
        ArmFailure::Busy => Error::WatchpointHardwareBusy {
            thread: debug_thread_id(pid),
        },
        ArmFailure::Unsupported(description) => {
            Error::HardwareWatchpointsUnavailable(description.as_str().into())
        }
        ArmFailure::System(error) => backend_error(LinuxError::System(*error)),
    }
}

/// Installs `plan` in one stopped thread's debug registers and reads it
/// back. A readback mismatch means the target silently ignored the writes.
pub(super) fn program_debug_registers(
    ptrace: &dyn LinuxTraceOps,
    pid: Pid,
    plan: &DebugRegisterPlan,
) -> std::result::Result<(), ArmFailure> {
    for (register, value) in plan.programming_sequence() {
        ptrace
            .write_debug_register(pid, register, value)
            .map_err(arm_failure)?;
    }
    let expected = std::iter::once((debug_registers::CONTROL_REGISTER, plan.control())).chain(
        plan.slots()
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.as_ref().map(|slot| (index, slot.chunk.address))),
    );
    for (register, value) in expected {
        let actual = ptrace
            .read_debug_register(pid, register)
            .map_err(arm_failure)?;
        if actual != value {
            return Err(ArmFailure::Unsupported(format!(
                "debug register {register} read back {actual:#x} after writing {value:#x}"
            )));
        }
    }
    Ok(())
}
