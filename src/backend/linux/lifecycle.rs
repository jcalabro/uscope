//! Starting, attaching to, and ending a traced process, and tracking its
//! threads, forks, and exec.

use std::collections::{BTreeMap, BTreeSet};

use super::signals::{Signal, WaitEvent};
use nix::errno::Errno;
use nix::libc;
use nix::unistd::Pid;

use crate::backend::process_start_time;
use crate::protocol::{
    DebuggerEvent, ExecutionId, ExitStatus, LaunchOptions, ProcessId, Reply, ResumeScope, StopId,
    StopReason,
};
use crate::{Error, LoadedModule, Result};

use super::breakpoints::install_logical_breakpoint;
use super::native::{LinuxTraceOps, wait_for};
use super::{
    ActiveExecution, ActiveKind, ClassifiedStop, Controller, ExpectedStop, Inferior,
    InferiorOrigin, LinuxError, NativeThreadState, StopBarrier, TraceThread, Waiter, backend_error,
    debug_thread_id, exception_info, process_id,
};

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn launch(&mut self, options: LaunchOptions, reply: Reply<ExecutionId>) {
        if self.inferior.is_some() || self.launch_reply.is_some() {
            let _ = reply.send(Err(Error::AlreadyRunning));
            return;
        }

        let stop_at_entry = options.stop_at_entry;
        match self.ptrace.spawn(&self.executable, options) {
            Ok(pid) => {
                let waiter = match self.ptrace.spawn_waiter(self.message_sender.clone()) {
                    Ok(waiter) => waiter,
                    Err(error) => {
                        let _ = self.ptrace.kill(pid, Signal::SIGKILL);
                        let _ = self.ptrace.reap(pid);
                        let _ = reply.send(Err(error));
                        return;
                    }
                };
                let process_id = process_id(pid);
                let execution_id = ExecutionId::new(1);
                self.reset_breakpoint_hit_counts();
                self.inferior = Some(Inferior {
                    active: Some(ActiveExecution {
                        id: execution_id,
                        kind: ActiveKind::Launch,
                        scope: ResumeScope::Process(process_id),
                        resume_threads: BTreeSet::from([pid]),
                    }),
                    next_execution: 1,
                    // Like a pause requested during launch, an entry stop
                    // completes at the initial exec stop.
                    barrier: stop_at_entry.then_some(StopBarrier::visible(pid, StopReason::Entry)),
                    ..Inferior::new(
                        InferiorOrigin::Launched,
                        pid,
                        LoadedModule::main(self.module_image.id(), 0),
                        BTreeMap::from([(pid, TraceThread::starting(ExpectedStop::InitialExec))]),
                        Some(waiter),
                    )
                });
                self.launch_reply = Some(reply);
                self.bump_revision();
                let _ = self.events.send(DebuggerEvent::InferiorLaunched {
                    revision: self.revision,
                    process_id,
                    execution_id,
                });
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    pub(super) fn attach(&mut self, requested: ProcessId, reply: Reply<StopId>) {
        if self.inferior.is_some() || self.launch_reply.is_some() || self.attach_reply.is_some() {
            let _ = reply.send(Err(Error::AlreadyRunning));
            return;
        }
        let Ok(raw) = i32::try_from(requested.get()) else {
            let _ = reply.send(Err(Error::InvalidProcessId(requested.get())));
            return;
        };
        if raw <= 0 {
            let _ = reply.send(Err(Error::InvalidProcessId(requested.get())));
            return;
        }
        let requested_pid = Pid::from_raw(raw);
        let tgid = match self.ptrace.thread_group_id(requested_pid) {
            Ok(tgid) => tgid,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };

        let mut seized = BTreeSet::new();
        let result = (|| -> Result<()> {
            for _ in 0..128 {
                let observed = self.ptrace.process_threads(tgid)?;
                for tid in observed {
                    if !seized.contains(&tid) && self.ptrace.seize(tid)? {
                        seized.insert(tid);
                    }
                }
                let current = self.ptrace.process_threads(tgid)?;
                if current.iter().all(|tid| seized.contains(tid)) {
                    seized.retain(|tid| current.binary_search(tid).is_ok());
                    return Ok(());
                }
            }
            Err(backend_error(LinuxError::AttachThreadsUnstable))
        })();
        if let Err(error) = result {
            self.rollback_seized(&seized);
            let _ = reply.send(Err(error));
            return;
        }
        if self
            .expected_process_start_time
            .is_some_and(|expected| process_start_time(tgid.as_raw()) != Some(expected))
        {
            self.rollback_seized(&seized);
            let _ = reply.send(Err(Error::TargetChangedDuringAttach));
            return;
        }
        if seized.is_empty() {
            let _ = reply.send(Err(Error::NotRunning));
            return;
        }

        let waiter = match self.ptrace.spawn_waiter(self.message_sender.clone()) {
            Ok(waiter) => waiter,
            Err(error) => {
                self.rollback_seized(&seized);
                let _ = reply.send(Err(error));
                return;
            }
        };
        let threads = seized
            .iter()
            .map(|&pid| (pid, TraceThread::starting(ExpectedStop::InitialAttach)))
            .collect();
        self.reset_breakpoint_hit_counts();
        self.inferior = Some(Inferior::new(
            InferiorOrigin::Attached,
            tgid,
            LoadedModule::main(self.module_image.id(), 0),
            threads,
            Some(waiter),
        ));
        self.attach_reply = Some(reply);

        for tid in seized {
            match self.ptrace.interrupt(tid) {
                Ok(true) => {}
                // The thread is exiting; its exit status retires it.
                Ok(false) => {
                    let inferior = self.inferior.as_mut().expect("attached inferior exists");
                    inferior.threads.remove(&tid);
                    inferior.retired_threads.insert(tid);
                }
                Err(error) => {
                    self.fail_inferior(error);
                    return;
                }
            }
        }
        // The attach stop is presented from the leader when it survives.
        let inferior = self.inferior.as_mut().expect("attached inferior exists");
        let Some(triggering_thread) = inferior
            .threads
            .contains_key(&tgid)
            .then_some(tgid)
            .or_else(|| inferior.threads.keys().next().copied())
        else {
            self.fail_inferior(Error::NotRunning);
            return;
        };
        inferior.barrier = Some(StopBarrier::visible(triggering_thread, StopReason::Attach));
    }

    pub(super) fn rollback_seized(&self, seized: &BTreeSet<Pid>) {
        for &pid in seized {
            let _ = self.ptrace.interrupt(pid);
        }
        for &pid in seized {
            loop {
                match wait_for(pid, libc::__WALL) {
                    Ok(Some(WaitEvent::Stopped(..) | WaitEvent::PtraceEvent(..))) => {
                        let _ = self.ptrace.detach(pid, None);
                        break;
                    }
                    Ok(Some(WaitEvent::Exited(..) | WaitEvent::Signaled(..)))
                    | Err(Errno::ECHILD) => {
                        break;
                    }
                    Ok(_) | Err(Errno::EINTR) => {}
                    Err(_) => break,
                }
            }
        }
    }

    pub(super) fn handle_initial_stop(&mut self, pid: Pid) -> Result<()> {
        self.ptrace.set_options(pid, true)?;
        let load_bias = self.ptrace.load_bias(
            pid,
            &self.executable,
            &self.executable_data,
            self.executable_identity,
        )?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.loaded_module = LoadedModule::main(self.module_image.id(), load_bias);
        self.modules
            .get_mut(&crate::ModuleId::new(0))
            .expect("main module is registered")
            .loaded = inferior.loaded_module;

        for breakpoint in &self.breakpoints {
            install_logical_breakpoint(&self.ptrace, inferior, breakpoint)?;
        }

        let execution_id = inferior.active.as_ref().expect("launch is active").id;
        let process_id = process_id(inferior.tgid);
        let pause_requested = inferior.barrier.is_some();
        let generation = inferior.watch.generation;
        let thread = inferior.thread_mut(pid)?;
        thread.expected = ExpectedStop::None;
        // A freshly executed image starts with empty debug registers.
        thread.armed = Some(generation);
        if pause_requested {
            // A pause requested during launch completes at this stop instead
            // of letting the new image run first.
            thread.state = NativeThreadState::Stopped;
            if let Some(reply) = self.launch_reply.take() {
                let _ = reply.send(Ok(execution_id));
            }
            return self.finish_barrier_if_ready();
        }

        self.ptrace.continue_execution(pid, None)?;
        thread.state = NativeThreadState::Running;
        self.bump_revision();
        if let Some(reply) = self.launch_reply.take() {
            let _ = reply.send(Ok(execution_id));
        }
        let _ = self.events.send(DebuggerEvent::InferiorContinued {
            revision: self.revision,
            process_id,
            execution_id,
            resumed: ResumeScope::Process(process_id),
        });
        Ok(())
    }

    pub(super) fn handle_initial_attach_stop(&mut self, pid: Pid) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let thread = inferior.thread_mut(pid)?;
        thread.state = NativeThreadState::Stopped;
        thread.expected = ExpectedStop::None;
        self.finish_barrier_if_ready()
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn handle_thread_start(&mut self, pid: Pid) -> Result<()> {
        let exit_kill = self
            .inferior
            .as_ref()
            .is_some_and(|inferior| inferior.origin == InferiorOrigin::Launched);
        self.ptrace.set_options(pid, exit_kill)?;
        let arm_failure = self.arm_new_thread(pid);
        let (process_id, barrier_active, should_resume) = {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            let thread = inferior.thread_mut(pid)?;
            thread.state = NativeThreadState::Stopped;
            thread.expected = ExpectedStop::None;
            // A thread created while a breakpoint site is lifted for a repair
            // waits for the repair; resuming execution then resumes it.
            let should_resume = inferior.repairs.is_empty()
                && inferior
                    .active
                    .as_ref()
                    .is_some_and(|active| active.resume_threads.contains(&pid));
            (
                process_id(inferior.tgid),
                inferior.barrier.is_some(),
                should_resume,
            )
        };
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::ThreadStarted {
            revision: self.revision,
            process_id,
            thread_id: debug_thread_id(pid),
        });
        if let Some(error) = arm_failure {
            // The new thread stays stopped: running it unarmed would silently
            // miss accesses the user asked to watch.
            return self.begin_visible_stop(
                pid,
                StopReason::WatchpointArmFailed {
                    thread_id: debug_thread_id(pid),
                    description: error.to_string().into(),
                },
            );
        }
        if barrier_active {
            self.finish_barrier_if_ready()
        } else if should_resume {
            self.continue_thread(pid)
        } else {
            Ok(())
        }
    }

    pub(super) fn initialize_attached_inferior(&mut self) -> Result<()> {
        let pid = self.inferior.as_ref().ok_or(Error::NotRunning)?.tgid;
        let load_bias = self.ptrace.load_bias(
            pid,
            &self.executable,
            &self.executable_data,
            self.executable_identity,
        )?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.loaded_module = LoadedModule::main(self.module_image.id(), load_bias);
        self.modules
            .get_mut(&crate::ModuleId::new(0))
            .expect("main module is registered")
            .loaded = inferior.loaded_module;
        for breakpoint in &self.breakpoints {
            install_logical_breakpoint(&self.ptrace, inferior, breakpoint)?;
        }
        self.clear_attached_debug_registers()
    }

    pub(super) fn handle_ptrace_event(&mut self, pid: Pid, event: i32) -> Result<()> {
        match event {
            libc::PTRACE_EVENT_CLONE => {
                self.inferior
                    .as_mut()
                    .and_then(|inferior| inferior.threads.get_mut(&pid))
                    .ok_or(Error::NotRunning)?
                    .state = NativeThreadState::Stopped;
                self.handle_clone_event(pid)
            }
            libc::PTRACE_EVENT_FORK => {
                self.inferior
                    .as_mut()
                    .ok_or(Error::NotRunning)?
                    .thread_mut(pid)?
                    .state = NativeThreadState::Stopped;
                self.handle_fork_event(pid)
            }
            libc::PTRACE_EVENT_EXEC => {
                self.inferior
                    .as_mut()
                    .and_then(|inferior| inferior.threads.get_mut(&pid))
                    .ok_or(Error::NotRunning)?
                    .state = NativeThreadState::Stopped;
                self.handle_exec_event(pid)
            }
            libc::PTRACE_EVENT_EXIT => {
                let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
                inferior.thread_mut(pid)?.state = NativeThreadState::Exiting;
                self.ptrace.continue_execution(pid, None)
            }
            other => self.begin_visible_stop(
                pid,
                StopReason::Unclassifiable {
                    description: format!("unsupported ptrace event {other}").into(),
                },
            ),
        }
    }

    pub(super) fn handle_clone_event(&mut self, parent: Pid) -> Result<()> {
        let child = Pid::from_raw(
            i32::try_from(self.ptrace.event_message(parent)?)
                .map_err(|_| Error::AddressOverflow)?,
        );
        let child_tgid = self.ptrace.thread_group_id(child)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        if child_tgid != inferior.tgid {
            // The killed child's exit status is still delivered.
            inferior.unowned_stops.remove(&child);
            inferior.retired_threads.insert(child);
            let _ = self.ptrace.kill(child, Signal::SIGKILL);
            return self.begin_visible_stop(
                parent,
                StopReason::Unclassifiable {
                    description: backend_error(LinuxError::UnsupportedClone(child_tgid.as_raw()))
                        .to_string()
                        .into(),
                },
            );
        }
        assert!(
            inferior
                .threads
                .insert(child, TraceThread::starting(ExpectedStop::None))
                .is_none(),
            "clone TID is unique"
        );
        // A new thread runs with its siblings, unless the execution resumed
        // one thread alone.
        if let Some(active) = inferior.active.as_mut()
            && matches!(active.scope, ResumeScope::Process(_))
        {
            active.resume_threads.insert(child);
        }
        let pending = inferior.unowned_stops.remove(&child);
        let barrier_active = inferior.barrier.is_some();
        if barrier_active {
            inferior.thread_mut(parent)?.state = NativeThreadState::Stopped;
        } else {
            self.restart_after_internal(parent)?;
        }
        if let Some(status) = pending {
            self.process_wait(status)?;
        }
        Ok(())
    }

    /// Releases a forked child, which the debugger does not follow, and
    /// resumes the parent.
    pub(super) fn handle_fork_event(&mut self, parent: Pid) -> Result<()> {
        let child = Pid::from_raw(
            i32::try_from(self.ptrace.event_message(parent)?)
                .map_err(|_| Error::AddressOverflow)?,
        );
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        if inferior.unowned_stops.remove(&child).is_some() {
            self.release_fork_child(child);
        } else {
            inferior.fork_children.insert(child);
        }
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        if inferior.barrier.is_some() {
            Ok(())
        } else {
            self.restart_after_internal(parent)
        }
    }

    /// Detaches a stopped fork child after removing the breakpoints it
    /// inherited, which would otherwise kill it with SIGTRAP once untraced.
    /// Debug registers are not inherited across fork. A child that cannot be
    /// cleaned is killed rather than released with traps in place.
    pub(super) fn release_fork_child(&mut self, child: Pid) {
        let Some(inferior) = self.inferior.as_mut() else {
            return;
        };
        let sites = inferior
            .breakpoints
            .iter()
            .filter(|(_, site)| site.installed)
            .map(|(&address, site)| (address, site.original_byte));
        let mut cleaned = Ok(());
        for (address, original_byte) in sites {
            cleaned = self
                .ptrace
                .read_word(child, address.get())
                .and_then(|word| {
                    let mut bytes = word.to_ne_bytes();
                    bytes[0] = original_byte;
                    self.ptrace
                        .write_word(child, address.get(), u64::from_ne_bytes(bytes))
                });
            if cleaned.is_err() {
                break;
            }
        }
        if cleaned
            .and_then(|()| self.ptrace.detach(child, None))
            .is_err()
        {
            let _ = self.ptrace.kill(child, Signal::SIGKILL);
            inferior.retired_threads.insert(child);
        }
    }

    pub(super) fn handle_exec_event(&mut self, pid: Pid) -> Result<()> {
        let old_tid = Pid::from_raw(
            i32::try_from(self.ptrace.event_message(pid)?).map_err(|_| Error::AddressOverflow)?,
        );
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let mut survivor = inferior
            .threads
            .remove(&old_tid)
            .or_else(|| inferior.threads.remove(&pid))
            .unwrap_or_else(|| TraceThread::starting(ExpectedStop::None));
        inferior
            .retired_threads
            .extend(inferior.threads.keys().copied());
        inferior.threads.clear();
        survivor.state = NativeThreadState::Stopped;
        survivor.expected = ExpectedStop::None;
        survivor.pending_signal = None;
        survivor.stopped_at_breakpoint = None;
        survivor.awaiting_breakpoint = None;
        survivor.debugger_stop_pending = false;
        // The execing thread takes over the leader's TID, whose exit the
        // kernel never reports.
        inferior.retired_threads.remove(&pid);
        inferior.threads.insert(pid, survivor);
        inferior.breakpoints.clear();
        inferior.repairs.clear();
        inferior.exec_unsupported = true;
        // exec(2) flushes every debug register; the new image's addresses
        // have no relation to the old watchpoints.
        self.discard_watchpoints();
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let generation = inferior.watch.generation;
        if let Some(thread) = inferior.threads.get_mut(&pid) {
            thread.armed = Some(generation);
            thread.watch_hits.clear();
        }
        self.begin_visible_stop(pid, StopReason::Exec)
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Collects SIGTRAPs an attached thread queued before it was interrupted.
    ///
    /// `PTRACE_INTERRUPT` stops a thread before it dequeues signals, so a
    /// watchpoint or breakpoint trap raised just before the interrupt is still
    /// queued at the stop. Publishing that stop would report the trap only
    /// after a later resume, possibly after its watchpoint was removed, and
    /// detaching would deliver it to an untraced process. Resuming such a
    /// thread without a signal makes it dequeue the trap into an ordinary
    /// signal-delivery stop before running any instruction. Returns whether
    /// any thread was resumed.
    pub(super) fn drain_queued_traps(&mut self) -> Result<bool> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        if inferior.origin != InferiorOrigin::Attached {
            // Launched threads stop with SIGSTOP, which the kernel dequeues
            // only after synchronous signals such as SIGTRAP.
            return Ok(false);
        }
        let candidates = inferior
            .threads
            .iter()
            .filter(|(_, thread)| {
                matches!(thread.state, NativeThreadState::Stopped)
                    && thread.pending_signal.is_none()
            })
            .map(|(&pid, _)| pid)
            .collect::<Vec<_>>();
        let mut resumed = false;
        for pid in candidates {
            if !self.ptrace.queued_trap(pid)? {
                continue;
            }
            self.ptrace.continue_execution(pid, None)?;
            inferior.thread_mut(pid)?.state = NativeThreadState::Running;
            resumed = true;
        }
        Ok(resumed)
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn handle_terminal(&mut self, pid: Pid, status: ExitStatus) -> Result<()> {
        let thread_id = debug_thread_id(pid);
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let exited = inferior
            .threads
            .remove(&pid)
            .ok_or(Error::UnknownThread(thread_id))?;
        let process_id = process_id(inferior.tgid);
        let execution = inferior.active.as_ref().map(|active| active.id);

        if inferior.threads.is_empty() {
            self.discard_watchpoints();
            self.abandon_fork_children();
            let mut inferior = self.inferior.take().expect("inferior exists");
            if let Some(waiter) = inferior.waiter.take() {
                waiter.join()?;
            }
            self.reset_runtime_modules();
            self.bump_revision();
            let _ = self.events.send(DebuggerEvent::InferiorExited {
                revision: self.revision,
                process_id,
                execution_id: execution,
                status: status.clone(),
            });
            // A process that exits before its first stop never completes a
            // pending launch or attach.
            let startup_failed = || backend_error(LinuxError::ExitedBeforeStop(status.clone()));
            if let Some(reply) = self.launch_reply.take() {
                let _ = reply.send(Err(startup_failed()));
            }
            if let Some(reply) = self.attach_reply.take() {
                let _ = reply.send(Err(startup_failed()));
            }
            if let Some(reply) = self.shutdown_reply.take() {
                let _ = reply.send(Ok(()));
            }
            return Ok(());
        }

        let owned_execution = inferior.active.as_ref().is_some_and(|active| {
            matches!(active.scope, ResumeScope::Thread(thread) if thread == thread_id)
                || matches!(active.kind, ActiveKind::Step { thread, .. } if thread == pid)
        });
        // A thread resumed alone to reach its awaited breakpoint holds its
        // siblings back; they must run once it is gone.
        let ran_alone = exited.awaiting_breakpoint.is_some() && inferior.active.is_some();
        let repair_interrupted = inferior.forget_thread(pid, &status);
        let barrier_active = inferior.barrier.is_some();
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::ThreadExited {
            revision: self.revision,
            process_id,
            thread_id,
            status: status.clone(),
        });
        // A shutdown only stops and releases threads; it never runs them.
        if self.shutting_down {
            return Ok(());
        }
        if barrier_active {
            if owned_execution
                && let Some(barrier) = self
                    .inferior
                    .as_mut()
                    .and_then(|inferior| inferior.barrier.as_mut())
                    .filter(|barrier| barrier.reason.is_none())
            {
                // An internal stop has no execution left to resume.
                barrier.reason = Some(StopReason::ThreadExited { thread_id, status });
            }
            return self.finish_barrier_if_ready();
        }
        if owned_execution {
            let reason = StopReason::ThreadExited { thread_id, status };
            let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
            // Every other thread stayed stopped during a single-thread
            // execution, and one of them presents the stop. During an
            // all-thread step they may all be running.
            if let Some(stopped) = inferior.threads.iter().find_map(|(&pid, thread)| {
                matches!(thread.state, NativeThreadState::Stopped).then_some(pid)
            }) {
                return self.begin_visible_stop(stopped, reason);
            }
            let running = *inferior.threads.keys().next().ok_or(Error::NotRunning)?;
            return self.raise_barrier(running, reason);
        }
        if repair_interrupted || ran_alone {
            return self.advance_execution();
        }
        Ok(())
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn begin_shutdown(&mut self, reply: Option<Reply<()>>) {
        self.shutting_down = true;
        self.shutdown_reply = reply;
        self.launch_reply
            .take()
            .map(|reply| reply.send(Err(Error::RequestCancelled)));
        self.attach_reply
            .take()
            .map(|reply| reply.send(Err(Error::RequestCancelled)));

        let Some(inferior) = self.inferior.as_ref() else {
            self.finish_shutdown(Ok(()));
            return;
        };
        if inferior.origin != InferiorOrigin::Attached {
            if let Err(error) = self.kill_inferior() {
                self.finish_shutdown(Err(error));
            }
            return;
        }
        // An attached process is detached once every thread is stopped.
        let running = inferior
            .threads
            .iter()
            .filter_map(|(&pid, thread)| {
                (!matches!(thread.state, NativeThreadState::Stopped)).then_some(pid)
            })
            .collect::<Vec<_>>();
        for tid in running {
            match self.ptrace.interrupt(tid) {
                Ok(true) => {}
                // The thread is exiting; its exit status retires it.
                Ok(false) => {
                    let inferior = self.inferior.as_mut().expect("attached inferior exists");
                    inferior.threads.remove(&tid);
                    inferior.retired_threads.insert(tid);
                }
                Err(error) => {
                    self.finish_shutdown(Err(error));
                    return;
                }
            }
        }
        if let Err(error) = self.detach_when_stopped() {
            self.finish_shutdown(Err(error));
        }
    }

    /// Sends the shutdown reply, if a client is waiting for one.
    pub(super) fn finish_shutdown(&mut self, result: Result<()>) {
        if let Some(reply) = self.shutdown_reply.take() {
            let _ = reply.send(result);
        }
    }

    /// Handles a wait status for a pid that is not a live thread of the
    /// inferior: a retired thread's late exit, a fork child, or a new
    /// thread's first stop arriving before its clone event. Returns whether
    /// the status needed nothing more.
    pub(super) fn absorb_untracked_wait(&mut self, status: &WaitEvent) -> bool {
        let pid = status.pid();
        let Some(inferior) = self.inferior.as_mut() else {
            return false;
        };
        if inferior.threads.contains_key(&pid) {
            return false;
        }
        match status {
            WaitEvent::Exited(..) | WaitEvent::Signaled(..) => {
                inferior.retired_threads.remove(&pid);
                inferior.fork_children.remove(&pid);
                inferior.unowned_stops.remove(&pid);
            }
            WaitEvent::Stopped(..) | WaitEvent::PtraceEvent(_, _, libc::PTRACE_EVENT_STOP) => {
                if inferior.fork_children.remove(&pid) {
                    self.release_fork_child(pid);
                } else {
                    inferior.unowned_stops.insert(pid, *status);
                }
            }
            _ => return false,
        }
        true
    }

    /// Releases fork children whose initial stop arrived and kills those
    /// still on their way to it. Once the process has exited no event will
    /// announce them, and a traced child left stopped would keep the waiter
    /// from ever finishing.
    fn abandon_fork_children(&mut self) {
        let Some(inferior) = self.inferior.as_mut() else {
            return;
        };
        let stopped = std::mem::take(&mut inferior.unowned_stops);
        let pending = std::mem::take(&mut inferior.fork_children);
        for &pid in stopped.keys() {
            self.release_fork_child(pid);
        }
        for pid in pending {
            let _ = self.ptrace.kill(pid, Signal::SIGKILL);
        }
    }

    pub(super) fn handle_shutdown_wait(&mut self, status: WaitEvent) -> bool {
        let attached = self
            .inferior
            .as_ref()
            .is_some_and(|inferior| inferior.origin == InferiorOrigin::Attached);
        if self.absorb_untracked_wait(&status) {
            if attached && let Err(error) = self.detach_when_stopped() {
                self.finish_shutdown(Err(error));
                return false;
            }
            return self.inferior.is_some();
        }
        if attached {
            return self.handle_detach_wait(status);
        }
        let result = match status {
            WaitEvent::Exited(pid, code) => {
                self.handle_terminal(pid, ExitStatus::Code(i64::from(code)))
            }
            WaitEvent::Signaled(pid, signal, _) => {
                self.handle_terminal(pid, ExitStatus::Terminated(exception_info(signal)))
            }
            WaitEvent::PtraceEvent(pid, _, event) if event == libc::PTRACE_EVENT_EXIT => {
                self.ptrace.continue_during_shutdown(pid)
            }
            WaitEvent::Stopped(pid, _) | WaitEvent::PtraceEvent(pid, _, _) => self
                .kill_inferior()
                .and_then(|()| self.ptrace.continue_during_shutdown(pid)),
            other => Err(backend_error(LinuxError::UnexpectedWait(format!(
                "{other:?}"
            )))),
        };
        if let Err(error) = result {
            self.finish_shutdown(Err(error));
            return false;
        }
        self.inferior.is_some()
    }

    pub(super) fn handle_detach_wait(&mut self, status: WaitEvent) -> bool {
        let result = match status {
            WaitEvent::Exited(pid, code) => {
                self.handle_terminal(pid, ExitStatus::Code(i64::from(code)))
            }
            WaitEvent::Signaled(pid, signal, _) => {
                self.handle_terminal(pid, ExitStatus::Terminated(exception_info(signal)))
            }
            WaitEvent::PtraceEvent(pid, _, event) if event == libc::PTRACE_EVENT_EXIT => {
                self.ptrace.continue_execution(pid, None)
            }
            WaitEvent::PtraceEvent(pid, _, event) if event == libc::PTRACE_EVENT_CLONE => {
                self.handle_clone_during_detach(pid)
            }
            WaitEvent::PtraceEvent(pid, _, event) if event == libc::PTRACE_EVENT_FORK => {
                self.handle_fork_during_detach(pid)
            }
            WaitEvent::Stopped(pid, signal) => {
                let classified = self.classify_stop(pid, signal);
                let inferior = self.inferior.as_mut().ok_or(Error::NotRunning);
                inferior.and_then(|inferior| {
                    let thread = inferior.thread_mut(pid)?;
                    thread.state = NativeThreadState::Stopped;
                    if let ClassifiedStop::SignalDelivery(pending) = classified {
                        thread.pending_signal = Some(pending);
                    }
                    Ok(())
                })
            }
            WaitEvent::PtraceEvent(pid, _, _) => {
                let inferior = self.inferior.as_mut().ok_or(Error::NotRunning);
                inferior.and_then(|inferior| {
                    inferior.thread_mut(pid)?.state = NativeThreadState::Stopped;
                    Ok(())
                })
            }
            other => Err(backend_error(LinuxError::UnexpectedWait(format!(
                "{other:?}"
            )))),
        };
        if let Err(error) = result.and_then(|()| self.detach_when_stopped()) {
            self.finish_shutdown(Err(error));
            return false;
        }
        self.inferior.is_some()
    }

    /// Detaches once every live thread is stopped, every fork child has
    /// been released, and no thread holds a queued trap. A drained thread's
    /// next stop retries the detach.
    pub(super) fn detach_when_stopped(&mut self) -> Result<()> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(());
        };
        if inferior.threads.is_empty() {
            // Every thread exited while the session ended; once their exits
            // are collected nothing remains to release.
            if inferior.retired_threads.is_empty() && inferior.fork_children.is_empty() {
                self.forget_vanished_inferior();
            }
            return Ok(());
        }
        let ready = inferior.fork_children.is_empty()
            && inferior
                .threads
                .values()
                .all(|thread| matches!(thread.state, NativeThreadState::Stopped));
        if ready && !self.drain_queued_traps()? {
            self.detach_inferior();
        }
        Ok(())
    }

    /// Ends the session of an attached process whose threads all exited.
    fn forget_vanished_inferior(&mut self) {
        let Some(mut inferior) = self.inferior.take() else {
            return;
        };
        let outcome = inferior.waiter.take().map_or(Ok(()), Waiter::stop_and_join);
        self.reset_runtime_modules();
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::InferiorDetached {
            revision: self.revision,
            process_id: process_id(inferior.tgid),
        });
        self.finish_shutdown(outcome);
    }

    /// Records a fork child announced while detaching; its initial stop
    /// releases it, and the detach waits for that.
    fn handle_fork_during_detach(&mut self, parent: Pid) -> Result<()> {
        let child = Pid::from_raw(
            i32::try_from(self.ptrace.event_message(parent)?)
                .map_err(|_| Error::AddressOverflow)?,
        );
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.thread_mut(parent)?.state = NativeThreadState::Stopped;
        if inferior.unowned_stops.remove(&child).is_some() {
            self.release_fork_child(child);
        } else {
            inferior.fork_children.insert(child);
        }
        Ok(())
    }

    pub(super) fn handle_clone_during_detach(&mut self, parent: Pid) -> Result<()> {
        let child = Pid::from_raw(
            i32::try_from(self.ptrace.event_message(parent)?)
                .map_err(|_| Error::AddressOverflow)?,
        );
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.thread_mut(parent)?.state = NativeThreadState::Stopped;
        // The child's first stop may already have arrived.
        let stopped = inferior.unowned_stops.remove(&child).is_some();
        let thread = inferior
            .threads
            .entry(child)
            .or_insert_with(|| TraceThread::starting(ExpectedStop::None));
        if stopped {
            thread.state = NativeThreadState::Stopped;
        }
        Ok(())
    }

    /// Releases an attached process and publishes the detach.
    ///
    /// The tracer is exiting, so every step is attempted even if an earlier
    /// one fails: stopping early would leave the kernel to release threads
    /// with breakpoints or debug registers still armed. The first failure is
    /// reported in the shutdown reply.
    pub(super) fn detach_inferior(&mut self) {
        let mut first_error = self.disarm_for_detach().err();
        let mut record = |result: Result<()>| {
            if let Err(error) = result {
                first_error.get_or_insert(error);
            }
        };
        let mut inferior = self.inferior.take().expect("attached inferior exists");
        let installed = inferior
            .breakpoints
            .iter()
            .filter_map(|(&address, site)| site.installed.then_some(address))
            .collect::<Vec<_>>();
        let pid = inferior.memory_thread();
        for address in installed {
            record(
                self.ptrace
                    .remove_breakpoint(pid, &mut inferior.breakpoints, address),
            );
        }
        if let Some(waiter) = inferior.waiter.take() {
            record(waiter.stop_and_join());
        }
        for (&pid, thread) in &inferior.threads {
            let signal = thread
                .pending_signal
                .map(|pending| pending.signal)
                .filter(|signal| self.signals.get(*signal).pass);
            record(self.ptrace.detach(pid, signal));
        }
        if !inferior.watch.watchpoints.is_empty() {
            self.publish_watchpoints_changed();
        }
        self.reset_runtime_modules();
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::InferiorDetached {
            revision: self.revision,
            process_id: process_id(inferior.tgid),
        });
        self.finish_shutdown(first_error.map_or(Ok(()), Err));
    }

    pub(super) fn kill_inferior(&self) -> Result<()> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(());
        };
        self.ptrace.kill(inferior.tgid, Signal::SIGKILL)
    }

    pub(super) fn fail_inferior(&mut self, error: Error) {
        if let Some(reply) = self.launch_reply.take() {
            let _ = reply.send(Err(error));
        } else if let Some(reply) = self.attach_reply.take() {
            let _ = reply.send(Err(error));
            self.begin_shutdown(None);
            return;
        }
        if self
            .inferior
            .as_ref()
            .is_some_and(|inferior| inferior.origin == InferiorOrigin::Attached)
        {
            self.begin_shutdown(None);
        } else {
            let _ = self.kill_inferior();
        }
    }
}
