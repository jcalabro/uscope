//! All-stop execution control: resuming threads, repairing breakpoint
//! sites, and publishing coherent stops.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use nix::sys::signal::Signal as NixSignal;
use nix::unistd::Pid;

use crate::protocol::{
    DebuggerEvent, ExceptionDisposition, ExecutionId, ProcessId, Reply, ResumeScope, StepKind,
    StopId, StopReason, WatchpointId,
};
use crate::{Error, Result, StackFrameId, VirtualAddress};

use super::breakpoints::remove_breakpoint_owner_from;
use super::classify::{format_raw_stop, visible_stop_priority};
use super::native::{LinuxTraceOps, is_vanished_tracee};
use super::{
    ActiveExecution, ActiveKind, BreakpointOwner, ClassifiedStop, Controller, ExpectedStop,
    Inferior, InferiorOrigin, LinuxError, NativeThreadState, PendingSignal, PublicStop,
    RepairGroup, Resume, StopBarrier, allocate_stop_id, backend_error, debug_thread_id,
    exception_info, pending_exception_info, process_id, scoped_threads, validate_process,
    validate_public_stop, validate_resumable, validate_stopped_thread,
};

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn resume(
        &mut self,
        process_id: ProcessId,
        stop_id: StopId,
        scope: ResumeScope,
        exception: ExceptionDisposition,
        reply: Reply<ExecutionId>,
    ) {
        let result =
            self.begin_execution(process_id, stop_id, scope, ActiveKind::Continue, exception);
        self.reply_execution(result, scope, reply);
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "a step request names its stop, thread, frame, kind, and exception disposition"
    )]
    pub(super) fn step(
        &mut self,
        process_id: ProcessId,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
        kind: StepKind,
        exception: ExceptionDisposition,
        reply: Reply<ExecutionId>,
    ) {
        let scope = ResumeScope::Thread(debug_thread_id(pid));
        // A stale or invalid request fails before any unwinding can.
        let valid = self
            .inferior
            .as_ref()
            .ok_or(Error::NotRunning)
            .and_then(|inferior| {
                validate_process(inferior, process_id)?;
                validate_public_stop(inferior, Some(stop_id))?;
                validate_resumable(inferior)?;
                validate_stopped_thread(inferior, pid)?;
                if frame.get() != 0 && kind != StepKind::Out {
                    return Err(Error::FrameStepUnsupported(
                        "only stepping out applies to an outer frame; other steps begin at the innermost frame"
                            .into(),
                    ));
                }
                Ok(())
            });
        if let Err(error) = valid {
            let _ = reply.send(Err(error));
            return;
        }
        match self.try_virtual_step(process_id, stop_id, pid, kind) {
            Ok(Some((execution, stopped))) => {
                // Acknowledge the step before publishing the stop it caused.
                let _ = reply.send(Ok(execution));
                let _ = self.events.send(stopped);
                return;
            }
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
            Ok(None) => {}
        }
        let result = self.step_start(pid, kind, frame).and_then(|start| {
            self.begin_execution(
                process_id,
                stop_id,
                scope,
                ActiveKind::Step {
                    thread: pid,
                    kind,
                    start,
                },
                exception,
            )
        });
        self.reply_execution(result, scope, reply);
    }

    pub(super) fn pause(&mut self, process_id: ProcessId, reply: Reply<ExecutionId>) {
        let result = self.begin_pause(process_id);
        let _ = reply.send(result);
    }

    pub(super) fn begin_execution(
        &mut self,
        requested_process: ProcessId,
        stop_id: StopId,
        scope: ResumeScope,
        kind: ActiveKind,
        exception: ExceptionDisposition,
    ) -> Result<ExecutionId> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        validate_process(inferior, requested_process)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_resumable(inferior)?;
        scoped_threads(inferior, scope)?;
        self.sync_debug_registers()?;
        self.refresh_watch_baselines();
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;

        let resume_threads = scoped_threads(inferior, scope)?;
        inferior.next_execution = inferior.next_execution.wrapping_add(1);
        let execution_id = ExecutionId::new(inferior.next_execution);
        let owner = BreakpointOwner::Plan(execution_id);
        let mut installed = Vec::new();
        if let ActiveKind::Step { start, .. } = &kind {
            for &address in &start.plan_addresses {
                if let Err(error) = self.ptrace.install_breakpoint(
                    inferior.tgid,
                    &mut inferior.breakpoints,
                    address,
                    owner,
                ) {
                    for address in installed.into_iter().rev() {
                        if let Err(recovery) =
                            remove_breakpoint_owner_from(&self.ptrace, inferior, address, owner)
                        {
                            let _ = self.ptrace.kill(inferior.tgid, NixSignal::SIGKILL);
                            return Err(backend_error(LinuxError::ResumeRecovery {
                                cause: error.to_string(),
                                recovery: recovery.to_string(),
                            }));
                        }
                    }
                    return Err(error);
                }
                installed.push(address);
            }
        }
        let mut suppressed = Vec::new();
        if exception == ExceptionDisposition::Suppress {
            for &pid in &resume_threads {
                let thread = inferior.thread_mut(pid)?;
                if let Some(pending) = thread.pending_signal.take() {
                    suppressed.push((pid, pending));
                }
            }
        }
        inferior.public_stop = None;
        inferior.active = Some(ActiveExecution {
            id: execution_id,
            kind,
            scope,
            resume_threads,
        });
        inferior.repairs = collect_repairs(inferior);

        if let Err(error) = self.advance_execution() {
            self.restore_unconsumed_signals(&suppressed);
            let cause = error.to_string();
            if let Err(recovery) = self.recover_partial_resume() {
                let _ = self.kill_inferior();
                return Err(backend_error(LinuxError::ResumeRecovery {
                    cause,
                    recovery: recovery.to_string(),
                }));
            }
            return Err(error);
        }
        self.bump_revision();
        Ok(execution_id)
    }

    pub(super) fn restore_unconsumed_signals(&mut self, suppressed: &[(Pid, PendingSignal)]) {
        let Some(inferior) = self.inferior.as_mut() else {
            return;
        };
        for &(pid, pending) in suppressed {
            if let Some(thread) = inferior.threads.get_mut(&pid)
                && matches!(thread.state, NativeThreadState::Stopped)
                && thread.pending_signal.is_none()
            {
                thread.pending_signal = Some(pending);
            }
        }
    }

    pub(super) fn reply_execution(
        &self,
        result: Result<ExecutionId>,
        scope: ResumeScope,
        reply: Reply<ExecutionId>,
    ) {
        match result {
            Ok(execution_id) => {
                let process_id = self
                    .inferior
                    .as_ref()
                    .map(|inferior| process_id(inferior.tgid))
                    .expect("successful execution has an inferior");
                let _ = reply.send(Ok(execution_id));
                let _ = self.events.send(DebuggerEvent::InferiorContinued {
                    revision: self.revision,
                    process_id,
                    execution_id,
                    resumed: scope,
                });
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    pub(super) fn begin_pause(&mut self, requested_process: ProcessId) -> Result<ExecutionId> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        validate_process(inferior, requested_process)?;
        let execution_id = inferior.active.as_ref().ok_or(Error::AlreadyStopped)?.id;
        if let Some(barrier) = inferior.barrier.as_mut() {
            // An internal stop in progress becomes the requested pause
            // instead of resuming once it completes.
            barrier.reason.get_or_insert(StopReason::Pause);
            return Ok(execution_id);
        }

        // A launching thread has no stop to request: its initial exec stop
        // completes the barrier.
        let triggering_thread = inferior
            .threads
            .iter()
            .find_map(|(&pid, thread)| {
                (matches!(thread.state, NativeThreadState::Running)
                    || matches!(thread.expected, ExpectedStop::InitialExec))
                .then_some(pid)
            })
            .ok_or(Error::NotStopped)?;
        inferior.barrier = Some(StopBarrier {
            execution: Some(execution_id),
            triggering_thread,
            reason: Some(StopReason::Pause),
        });

        self.request_stops()?;
        self.finish_barrier_if_ready()?;
        Ok(execution_id)
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn advance_execution(&mut self) -> Result<()> {
        if !self
            .inferior
            .as_ref()
            .ok_or(Error::NotRunning)?
            .repairs
            .is_empty()
        {
            return self.start_next_repair();
        }

        let (kind, resume_threads) = {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            // A thread awaiting a breakpoint that has since been removed will
            // never trap there; resuming it alone would strand its siblings.
            for thread in inferior.threads.values_mut() {
                if thread.awaiting_breakpoint.is_some_and(|address| {
                    !inferior
                        .breakpoints
                        .get(&address)
                        .is_some_and(|site| site.installed)
                }) {
                    thread.awaiting_breakpoint = None;
                }
            }
            let active = inferior.active.as_ref().ok_or(Error::NotRunning)?;
            (active.kind.clone(), active.resume_threads.clone())
        };

        match kind {
            ActiveKind::Step { thread, kind, .. } => self.start_user_step(thread, kind),
            ActiveKind::Launch | ActiveKind::Continue => {
                if let Some(pid) = resume_threads.iter().copied().find(|pid| {
                    self.inferior
                        .as_ref()
                        .and_then(|inferior| inferior.threads.get(pid))
                        .is_some_and(|thread| thread.awaiting_breakpoint.is_some())
                }) {
                    self.resume_awaiting_thread(pid)
                } else {
                    for pid in resume_threads {
                        self.continue_thread(pid)?;
                    }
                    Ok(())
                }
            }
        }
    }

    pub(super) fn start_next_repair(&mut self) -> Result<()> {
        let next = {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            let group = inferior.repairs.front_mut().expect("repair group exists");
            if group.current.is_some() {
                return Ok(());
            }
            group.remaining.pop_front().map(|pid| {
                group.current = Some(pid);
                (pid, group.address, !group.site_removed)
            })
        };

        let Some((pid, address, remove_site)) = next else {
            self.finish_repair_group()?;
            return self.advance_execution();
        };
        // A thread stopped at a breakpoint cannot also hold a pending signal:
        // a signal stop rewinds no PC and repairs nothing.
        if self
            .inferior
            .as_ref()
            .ok_or(Error::NotRunning)?
            .thread(pid)?
            .pending_signal
            .is_some()
        {
            return Err(backend_error(LinuxError::UnexpectedWait(format!(
                "repair thread {pid} has a pending signal"
            ))));
        }

        if remove_site {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            self.ptrace
                .remove_breakpoint(inferior.tgid, &mut inferior.breakpoints, address)?;
            inferior
                .repairs
                .front_mut()
                .expect("repair group exists")
                .site_removed = true;
        }

        self.resume_native(
            pid,
            Resume::Step,
            false,
            ExpectedStop::BreakpointRepair { address },
        )
    }

    pub(super) fn finish_repair_group(&mut self) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let group = inferior.repairs.pop_front().expect("repair group exists");
        if group.site_removed {
            self.ptrace.reinstall_breakpoint(
                inferior.tgid,
                &mut inferior.breakpoints,
                group.address,
            )?;
        }
        Ok(())
    }

    pub(super) fn resume_awaiting_thread(&mut self, pid: Pid) -> Result<()> {
        let address = self
            .inferior
            .as_ref()
            .ok_or(Error::NotRunning)?
            .thread(pid)?
            .awaiting_breakpoint
            .expect("breakpoint is awaited");
        self.resume_native(
            pid,
            Resume::Continue,
            true,
            ExpectedStop::AwaitBreakpoint { address },
        )
    }

    pub(super) fn continue_thread(&mut self, pid: Pid) -> Result<()> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        if !matches!(inferior.thread(pid)?.state, NativeThreadState::Stopped) {
            return Ok(());
        }
        self.resume_native(pid, Resume::Continue, true, ExpectedStop::None)
    }

    /// Resumes one stopped thread and records the stop it should report next.
    ///
    /// With `deliver_signal`, the thread's pending signal is delivered and
    /// consumed; otherwise it stays pending. The thread's own stop reason is
    /// cleared either way, since it no longer describes a current stop.
    pub(super) fn resume_native(
        &mut self,
        pid: Pid,
        resume: Resume,
        deliver_signal: bool,
        expected: ExpectedStop,
    ) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let thread = inferior.thread_mut(pid)?;
        let signal = deliver_signal
            .then(|| thread.pending_signal.map(|pending| pending.signal))
            .flatten();
        let result = match resume {
            Resume::Continue => self.ptrace.continue_execution(pid, signal),
            Resume::Step => self.ptrace.step(pid, signal),
        };
        match result {
            Ok(()) => thread.state = NativeThreadState::Running,
            // Only SIGKILL takes a thread out of its ptrace-stop, typically
            // because a sibling resumed just before it called `exit_group`.
            // Its exit status is still delivered and retires it.
            Err(error) if is_vanished_tracee(&error) => thread.state = NativeThreadState::Exiting,
            Err(error) => return Err(error),
        }
        if deliver_signal {
            thread.pending_signal = None;
        }
        thread.expected = expected;
        thread.reason = None;
        Ok(())
    }

    pub(super) fn handle_classified_stop(&mut self, pid: Pid, stop: ClassifiedStop) -> Result<()> {
        {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            inferior.thread_mut(pid)?.state = NativeThreadState::Stopped;
        }

        match stop {
            ClassifiedStop::ThreadStart => self.handle_thread_start(pid),
            ClassifiedStop::DebuggerRequested => {
                let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
                let thread = inferior.thread_mut(pid)?;
                thread.reason = None;
                thread.debugger_stop_pending = false;
                if inferior.barrier.is_some() {
                    self.finish_barrier_if_ready()
                } else {
                    self.restart_after_internal(pid)
                }
            }
            ClassifiedStop::Breakpoint(address) => self.handle_breakpoint_stop(pid, address),
            ClassifiedStop::Watch(owners) => {
                let expected = self
                    .inferior
                    .as_ref()
                    .and_then(|inferior| inferior.threads.get(&pid))
                    .map(|thread| thread.expected.clone())
                    .ok_or(Error::NotRunning)?;
                self.finish_watched_instruction(pid, &expected, owners)
            }
            ClassifiedStop::Trace { watch } => self.handle_trace_stop(pid, watch),
            ClassifiedStop::SignalDelivery(pending) => self.handle_signal_stop(pid, pending),
            ClassifiedStop::GroupStop(signal) => {
                self.begin_visible_stop(pid, StopReason::Exception(exception_info(signal)))
            }
            ClassifiedStop::Unclassifiable(raw) => self.begin_visible_stop(
                pid,
                StopReason::Unclassifiable {
                    description: format_raw_stop(&raw).into(),
                },
            ),
        }
    }

    pub(super) fn handle_breakpoint_stop(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
    ) -> Result<()> {
        // A thread re-executing a trap after signal delivery interrupted its
        // repair reaches a hit that was already counted.
        let awaited = {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            let thread = inferior.thread_mut(pid)?;
            thread.stopped_at_breakpoint = Some(address);
            let awaited = thread.awaiting_breakpoint == Some(address)
                && matches!(thread.expected, ExpectedStop::AwaitBreakpoint { address: expected } if expected == address);
            if awaited {
                thread.awaiting_breakpoint = None;
                thread.expected = ExpectedStop::None;
            }
            awaited
        };
        if awaited {
            self.queue_repair(pid, address);
            return self.start_next_repair();
        }

        let stopping = self.record_breakpoint_hits(address);
        if !stopping.is_empty() {
            return self.begin_visible_stop(
                pid,
                StopReason::Breakpoint {
                    address,
                    hits: stopping,
                },
            );
        }

        let plan = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { thread, kind, .. } if *thread == pid => Some((active.id, *kind)),
                _ => None,
            })
            .filter(|(execution, _)| {
                self.inferior
                    .as_ref()
                    .and_then(|inferior| inferior.breakpoints.get(&address))
                    .is_some_and(|site| site.owners.contains(&BreakpointOwner::Plan(*execution)))
            });
        let Some((execution, kind)) = plan else {
            return self.resume_past_declined_hit(pid, address);
        };

        if kind != StepKind::Instruction {
            self.begin_epilogue_traversal(pid)?;
        }
        if self.source_step_returned_to_undescribed_code(pid, kind)? {
            self.cleanup_plan_breakpoints(execution)?;
            // A user breakpoint that declined this hit still owns the site.
            if self.release_lifted_site(pid, address)? {
                return self.continue_thread(pid);
            }
            self.queue_repair(pid, address);
            return self.start_next_repair();
        }
        if self.step_is_complete(pid, kind)? {
            self.cleanup_plan_breakpoints(execution)?;
            self.release_lifted_site(pid, address)?;
            return self.begin_visible_stop(pid, StopReason::Step { kind });
        }

        self.mark_epilogue_return_for_retirement(address);
        self.mark_return_guard_for_retirement(pid, address)?;
        self.queue_repair(pid, address);
        self.start_next_repair()
    }

    /// Clears `pid`'s pending repair once the site at `address` is gone, so
    /// it resumes from the original instruction. Returns whether the site
    /// was removed; a site that remains is still repaired on resume, so the
    /// thread never executes its trap a second time for the same arrival.
    fn release_lifted_site(&mut self, pid: Pid, address: VirtualAddress) -> Result<bool> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let removed = !inferior.breakpoints.contains_key(&address);
        if removed {
            inferior.thread_mut(pid)?.stopped_at_breakpoint = None;
        }
        Ok(removed)
    }

    /// Resumes a thread whose breakpoint hit no owner stops at, without
    /// publishing anything.
    ///
    /// The thread is repaired over the site before it runs on. A source
    /// step whose thread single-stepped onto the site completes there if the
    /// site is its destination. During a continue, every other thread must
    /// stop before the trap is lifted, or one could run past the site
    /// without reporting its own hit, so the repair happens at an internal
    /// stop.
    fn resume_past_declined_hit(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        let (barrier_active, step, execution) = {
            let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
            let active = inferior.active.as_ref().ok_or(Error::NotRunning)?;
            let step = match active.kind {
                ActiveKind::Step { thread, kind, .. } if thread == pid => Some(kind),
                _ => None,
            };
            (inferior.barrier.is_some(), step, active.id)
        };
        if barrier_active {
            // The thread is repaired when execution resumes after the stop
            // already in progress.
            return self.finish_barrier_if_ready();
        }
        if let Some(kind) = step {
            if kind != StepKind::Instruction && self.step_is_complete(pid, kind)? {
                return self.begin_visible_stop(pid, StopReason::Step { kind });
            }
            self.queue_repair(pid, address);
            return self.start_next_repair();
        }
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.barrier = Some(StopBarrier {
            execution: Some(execution),
            triggering_thread: pid,
            reason: None,
        });
        self.request_stops()?;
        self.finish_barrier_if_ready()
    }

    /// Completes an internal stop: every thread is stopped and nothing
    /// visible happened, so threads stopped at declined breakpoint hits are
    /// repaired and the active execution continues as if never interrupted.
    fn resume_after_internal_stop(&mut self) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.barrier = None;
        inferior.repairs = collect_repairs(inferior);
        self.advance_execution()
    }

    pub(super) fn handle_trace_stop(
        &mut self,
        pid: Pid,
        watch: BTreeSet<WatchpointId>,
    ) -> Result<()> {
        let expected = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.threads.get_mut(&pid))
            .map(|thread| std::mem::replace(&mut thread.expected, ExpectedStop::None))
            .ok_or(Error::NotRunning)?;
        if !watch.is_empty() {
            return self.finish_watched_instruction(pid, &expected, watch);
        }

        if self.pause_barrier_active() {
            return self.settle_trace_during_pause(pid, expected);
        }

        match expected {
            ExpectedStop::BreakpointRepair { address } => self.complete_repair(pid, address),
            ExpectedStop::UserStep { kind } => self.complete_user_step(pid, kind),
            other => self.begin_visible_stop(
                pid,
                StopReason::Unclassifiable {
                    description: format!("unexpected trace stop in {other:?}").into(),
                },
            ),
        }
    }

    /// Publishes a watchpoint stop for an instruction that accessed watched
    /// memory. When the instruction was a breakpoint repair step it has
    /// already executed, so its repair is complete; the site is reinstalled
    /// when the stop is published. A thread awaiting its breakpoint after
    /// signal delivery still re-traps there once resumed.
    pub(super) fn finish_watched_instruction(
        &mut self,
        pid: Pid,
        expected: &ExpectedStop,
        owners: BTreeSet<WatchpointId>,
    ) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        if let ExpectedStop::BreakpointRepair { address } = *expected {
            inferior.finish_current_repair(pid, address)?;
        }
        inferior.thread_mut(pid)?.watch_hits.extend(owners);
        self.begin_visible_stop(
            pid,
            StopReason::Watchpoint {
                hits: Arc::from([]),
            },
        )
    }

    pub(super) fn pause_barrier_active(&self) -> bool {
        self.inferior
            .as_ref()
            .and_then(|inferior| inferior.barrier.as_ref())
            .is_some_and(|barrier| barrier.reason == Some(StopReason::Pause))
    }

    pub(super) fn settle_trace_during_pause(
        &mut self,
        pid: Pid,
        expected: ExpectedStop,
    ) -> Result<()> {
        match expected {
            ExpectedStop::BreakpointRepair { address } => {
                self.inferior
                    .as_mut()
                    .ok_or(Error::NotRunning)?
                    .finish_current_repair(pid, address)?;
            }
            ExpectedStop::UserStep { .. } => {}
            other => {
                return self.begin_visible_stop(
                    pid,
                    StopReason::Unclassifiable {
                        description: format!("unexpected trace stop during pause in {other:?}")
                            .into(),
                    },
                );
            }
        }

        self.finish_barrier_if_ready()
    }

    pub(super) fn handle_signal_stop(&mut self, pid: Pid, pending: PendingSignal) -> Result<()> {
        let repair_address = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.threads.get(&pid))
            .and_then(|thread| match thread.expected {
                ExpectedStop::BreakpointRepair { address } => Some(address),
                _ => None,
            });
        if let Some(address) = repair_address {
            self.restore_active_breakpoints()?;
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            let thread = inferior.thread_mut(pid)?;
            thread.stopped_at_breakpoint = None;
            thread.awaiting_breakpoint = Some(address);
        }
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let thread = inferior.thread_mut(pid)?;
        thread.expected = ExpectedStop::None;
        thread.pending_signal = Some(pending);
        self.begin_visible_stop(pid, StopReason::Exception(pending_exception_info(pending)))
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn complete_repair(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        self.inferior
            .as_mut()
            .ok_or(Error::NotRunning)?
            .finish_current_repair(pid, address)?;

        let step_kind = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match active.kind {
                ActiveKind::Step { thread, kind, .. } if thread == pid => Some(kind),
                _ => None,
            });
        let group_done = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.repairs.front())
            .is_some_and(|group| group.remaining.is_empty());
        if group_done {
            self.finish_repair_group()?;
            if let Some(kind) = step_kind {
                return self.complete_user_step(pid, kind);
            }
            return self.advance_execution();
        }
        self.start_next_repair()
    }

    pub(super) fn queue_repair(&mut self, pid: Pid, address: VirtualAddress) {
        let inferior = self.inferior.as_mut().expect("inferior exists");
        if let Some(group) = inferior
            .repairs
            .iter_mut()
            .find(|group| group.address == address)
        {
            group.remaining.push_front(pid);
        } else {
            inferior.repairs.push_front(RepairGroup {
                address,
                remaining: VecDeque::from([pid]),
                current: None,
                site_removed: false,
            });
        }
    }

    /// Records a stop the client must see and begins stopping every other
    /// thread, unless a barrier is already doing so. Among coincident stops,
    /// the highest-priority reason is published.
    pub(super) fn begin_visible_stop(&mut self, pid: Pid, reason: StopReason) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let thread = inferior.thread_mut(pid)?;
        thread.state = NativeThreadState::Stopped;
        thread.reason = Some(reason.clone());
        thread.expected = ExpectedStop::None;

        if let Some(barrier) = inferior.barrier.as_mut() {
            // Any visible stop outranks an internal one.
            if barrier.reason.as_ref().is_none_or(|current| {
                visible_stop_priority(&reason) > visible_stop_priority(current)
            }) {
                barrier.triggering_thread = pid;
                barrier.reason = Some(reason);
            }
        } else {
            inferior.barrier = Some(StopBarrier {
                execution: inferior.active.as_ref().map(|active| active.id),
                triggering_thread: pid,
                reason: Some(reason),
            });
            self.request_stops()?;
        }
        self.finish_barrier_if_ready()
    }

    pub(super) fn recover_partial_resume(&mut self) -> Result<()> {
        self.restore_active_breakpoints()?;
        {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            let execution = inferior.active.as_ref().map(|active| active.id);
            let triggering_thread = inferior
                .threads
                .iter()
                .find_map(|(&pid, thread)| {
                    matches!(thread.state, NativeThreadState::Stopped).then_some(pid)
                })
                .or_else(|| inferior.threads.keys().next().copied())
                .ok_or(Error::NotRunning)?;
            let reason = StopReason::Unclassifiable {
                description: "execution resume failed; the debugger recovered to all-stop".into(),
            };
            inferior.thread_mut(triggering_thread)?.reason = Some(reason.clone());
            inferior.barrier = Some(StopBarrier {
                execution,
                triggering_thread,
                reason: Some(reason),
            });
        }
        self.request_stops()?;
        self.finish_barrier_if_ready()
    }

    pub(super) fn request_stops(&mut self) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let tgid = inferior.tgid;
        let attached = inferior.origin == InferiorOrigin::Attached;
        let running: Vec<_> = inferior
            .threads
            .iter()
            .filter_map(|(&pid, thread)| {
                matches!(thread.state, NativeThreadState::Running).then_some(pid)
            })
            .collect();
        for pid in running {
            let thread = inferior.thread_mut(pid)?;
            // A clone or another ptrace event can stop this thread and satisfy an
            // earlier barrier before its requested SIGSTOP is delivered. Standard
            // signals coalesce, so retain that outstanding request instead of
            // sending an indistinguishable duplicate for the next barrier.
            if !thread.debugger_stop_pending {
                if attached {
                    let _ = self.ptrace.interrupt(pid)?;
                } else {
                    self.ptrace.request_stop(tgid, pid)?;
                }
                thread.debugger_stop_pending = true;
            }
            thread.state = NativeThreadState::StopRequested;
        }
        Ok(())
    }

    pub(super) fn finish_barrier_if_ready(&mut self) -> Result<()> {
        let ready = self.inferior.as_ref().is_some_and(|inferior| {
            inferior.barrier.is_some()
                && inferior
                    .threads
                    .values()
                    .all(|thread| matches!(thread.state, NativeThreadState::Stopped))
        });
        if !ready {
            return Ok(());
        }
        if self.attach_reply.is_none() && self.drain_queued_traps()? {
            return Ok(());
        }
        if self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.barrier.as_ref())
            .is_some_and(|barrier| barrier.reason.is_none())
        {
            return self.resume_after_internal_stop();
        }

        if self.attach_reply.is_some() {
            self.initialize_attached_inferior()?;
        }
        self.restore_active_breakpoints()?;
        if let Some(execution) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .map(|active| active.id)
        {
            self.cleanup_plan_breakpoints(execution)?;
        }
        // Once the inferior has replaced its image via exec(2), the loaded
        // modules and loader rendezvous no longer correspond to `self.executable`.
        // Refreshing against the stale executable would read the new address
        // space through the old image and can fail the whole stop, killing the
        // inferior instead of surfacing the exec stop.
        let exec_replaced = self
            .inferior
            .as_ref()
            .is_some_and(|inferior| inferior.exec_unsupported);
        if !exec_replaced {
            self.refresh_modules()?;
        }
        self.evaluate_watchpoints()?;
        let (triggering_thread, reason) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.barrier.as_ref())
            .and_then(|barrier| Some((barrier.triggering_thread, barrier.reason.clone()?)))
            .expect("ready barrier publishes a reason");
        let presentation = self.presentation_for_thread(triggering_thread, Some(&reason))?;
        let stop_id = allocate_stop_id();
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        for thread in inferior.threads.values_mut() {
            thread.expected = ExpectedStop::None;
        }
        let barrier = inferior.barrier.take().expect("barrier exists");
        inferior.public_stop = Some(PublicStop {
            id: stop_id,
            triggering_thread: barrier.triggering_thread,
            reason: reason.clone(),
            presentations: BTreeMap::from([(triggering_thread, presentation)]),
            selected_frames: BTreeMap::new(),
        });
        inferior.selected_thread = Some(barrier.triggering_thread);
        inferior.active = None;
        let process_id = process_id(inferior.tgid);
        self.bump_revision();
        if self.attach_reply.is_some() {
            let _ = self.events.send(DebuggerEvent::InferiorAttached {
                revision: self.revision,
                process_id,
            });
        }
        let _ = self.events.send(DebuggerEvent::InferiorStopped {
            revision: self.revision,
            process_id,
            execution_id: barrier.execution,
            stop_id,
            thread_id: debug_thread_id(barrier.triggering_thread),
            reason,
        });
        if let Some(reply) = self.attach_reply.take() {
            let _ = reply.send(Ok(stop_id));
        }
        Ok(())
    }

    pub(super) fn restart_after_internal(&mut self, pid: Pid) -> Result<()> {
        let expected = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.threads.get(&pid))
            .map(|thread| thread.expected.clone())
            .ok_or(Error::NotRunning)?;
        match expected {
            ExpectedStop::UserStep { kind } => self.start_user_step(pid, kind),
            ExpectedStop::BreakpointRepair { address } => self.resume_native(
                pid,
                Resume::Step,
                false,
                ExpectedStop::BreakpointRepair { address },
            ),
            ExpectedStop::AwaitBreakpoint { .. } => self.resume_awaiting_thread(pid),
            ExpectedStop::InitialExec | ExpectedStop::InitialAttach | ExpectedStop::None => {
                self.continue_thread(pid)
            }
        }
    }
}

pub(super) fn collect_repairs(inferior: &Inferior) -> VecDeque<RepairGroup> {
    let active = inferior.active.as_ref().expect("execution is active");
    let mut grouped: BTreeMap<VirtualAddress, VecDeque<Pid>> = BTreeMap::new();
    for &pid in &active.resume_threads {
        if let Some(address) = inferior
            .threads
            .get(&pid)
            .and_then(|thread| thread.stopped_at_breakpoint)
        {
            grouped.entry(address).or_default().push_back(pid);
        }
    }
    grouped
        .into_iter()
        .map(|(address, remaining)| RepairGroup {
            address,
            remaining,
            current: None,
            site_removed: false,
        })
        .collect()
}
