//! Starting, attaching to, and ending a traced process, and tracking its
//! threads, forks, and exec.

use std::collections::{BTreeMap, BTreeSet};

use super::signals::{Signal, WaitEvent};
use nix::errno::Errno;
use nix::libc;
use nix::unistd::Pid;

use crate::protocol::{
    DebuggerEvent, ExceptionDisposition, ExecutionId, ExitStatus, LaunchOptions, ProcessId, Reply,
    ResumeScope, StopId, StopReason,
};
use crate::{Error, LoadedModule, Result, VirtualAddress};

use super::breakpoints::install_logical_breakpoint;
use super::native::{LinuxTraceOps, is_vanished_tracee};
use super::{
    ActiveExecution, ActiveKind, BREAKPOINT_OPCODE, ClassifiedStop, Controller, ExpectedStop,
    Inferior, InferiorOrigin, LinuxError, MemoryAccessError, NativeThreadState, Orphans, Start,
    StopBarrier, Terminating, TraceThread, Waiter, backend_error, debug_thread_id, exception_info,
    is_superseded, process_id,
};

/// How many times an attach may find threads it has not traced before it
/// gives up on a thread list that never settles.
const MAX_ATTACH_RESCANS: u32 = 128;

impl<P: LinuxTraceOps> Controller<P> {
    /// Begins a launch or attach, or holds it until the children the last
    /// program forked are released, since their waiter must finish before
    /// another waits.
    pub(super) fn start(&mut self, start: Start) {
        if self.orphans.is_some() && self.deferred_start.is_none() && !self.shutting_down {
            self.deferred_start = Some(start);
            return;
        }
        if self.inferior.is_some() || self.orphans.is_some() {
            start.fail(Error::AlreadyRunning);
            return;
        }
        match start {
            Start::Launch(options, reply) => self.launch(options, reply),
            Start::Attach(requested, reply) => self.attach(requested, reply),
            Start::LaunchByExec {
                requested,
                stop_at_entry,
                release,
                reply,
            } => self.launch_by_exec(requested, stop_at_entry, release, reply),
        }
    }

    fn launch(&mut self, options: LaunchOptions, reply: Reply<ExecutionId>) {
        let stop_at_entry = options.stop_at_entry;
        let started = self
            .ptrace
            .spawn(&self.executable, options)
            .and_then(|pid| {
                self.ptrace
                    .spawn_waiter(self.message_sender.clone())
                    .map(|waiter| (pid, waiter))
                    .inspect_err(|_| {
                        let _ = self.ptrace.kill(pid, Signal::SIGKILL);
                        let _ = self.ptrace.reap(pid);
                    })
            });
        match started {
            Ok((pid, waiter)) => {
                self.begin_launch(InferiorOrigin::Launched, pid, stop_at_entry, waiter, reply);
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    /// Seizes a process waiting to exec the executable and lets it, so the
    /// exec completes the launch as a spawned process's initial exec does.
    fn launch_by_exec(
        &mut self,
        requested: ProcessId,
        stop_at_entry: bool,
        release: Box<dyn FnOnce() + Send>,
        reply: Reply<ExecutionId>,
    ) {
        match self.seize_exec_target(requested) {
            Ok((pid, waiter)) => {
                let origin = InferiorOrigin::LaunchedByExec;
                self.begin_launch(origin, pid, stop_at_entry, waiter, reply);
                release();
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn seize_exec_target(&self, requested: ProcessId) -> Result<(Pid, Waiter)> {
        let pid = requested_pid(requested)?;
        if self.ptrace.thread_group_id(pid)? != pid {
            return Err(Error::InvalidProcessId(requested.get()));
        }
        if !self.ptrace.seize(pid, true)? {
            return Err(Error::NotRunning);
        }
        // A thread the process started before it was seized would exec
        // untraced.
        if self.ptrace.process_threads(pid)? != [pid] {
            self.rollback_seized(&BTreeSet::from([pid]));
            return Err(Error::ProcessHasThreads(requested.get()));
        }
        self.ptrace
            .spawn_waiter(self.message_sender.clone())
            .inspect_err(|_| self.rollback_seized(&BTreeSet::from([pid])))
            .map(|waiter| (pid, waiter))
    }

    /// Makes `pid`, which has yet to exec the executable, the inferior of a
    /// launch that its exec completes.
    fn begin_launch(
        &mut self,
        origin: InferiorOrigin,
        pid: Pid,
        stop_at_entry: bool,
        waiter: Waiter,
        reply: Reply<ExecutionId>,
    ) {
        let expected = if origin == InferiorOrigin::LaunchedByExec {
            ExpectedStop::AdoptedExec
        } else {
            ExpectedStop::InitialExec
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
            // Like a pause requested during launch, an entry stop completes
            // at the initial exec stop.
            barrier: stop_at_entry.then_some(StopBarrier::visible(pid, StopReason::Entry)),
            ..Inferior::new(
                origin,
                pid,
                LoadedModule::main(self.module_image.id(), 0),
                BTreeMap::from([(pid, TraceThread::starting(expected))]),
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

    fn attach(&mut self, requested: ProcessId, reply: Reply<StopId>) {
        let (tgid, seized, unseized, waiter) = match self.seize_process(requested) {
            Ok(seized) => seized,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let threads = seized
            .iter()
            .map(|&pid| (pid, TraceThread::starting(ExpectedStop::InitialAttach)))
            .collect();
        self.reset_breakpoint_hit_counts();
        let mut inferior = Inferior::new(
            InferiorOrigin::Attached,
            tgid,
            LoadedModule::main(self.module_image.id(), 0),
            threads,
            Some(waiter),
        );
        inferior.unseized_threads = unseized;
        self.inferior = Some(inferior);
        self.attach_reply = Some(reply);
        self.attach_rescans = 0;

        for tid in seized {
            if let Err(error) = self.interrupt_or_retire(tid) {
                self.fail_inferior(error);
                return;
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

    /// Seizes every listed thread of the process `requested` names, and
    /// starts the waiter that reports them. Returns the process, the threads
    /// seized, those that had finished exiting, and the waiter.
    fn seize_process(
        &self,
        requested: ProcessId,
    ) -> Result<(Pid, BTreeSet<Pid>, BTreeSet<Pid>, Waiter)> {
        let tgid = self.ptrace.thread_group_id(requested_pid(requested)?)?;
        let (seized, unseized) = self.seize_listed_threads(tgid)?;
        if self
            .expected_process_start_time
            .is_some_and(|expected| self.ptrace.process_start_time(tgid) != Some(expected))
        {
            self.rollback_seized(&seized);
            return Err(Error::TargetChangedDuringAttach);
        }
        if seized.is_empty() {
            return Err(Error::NotRunning);
        }
        let waiter = self
            .ptrace
            .spawn_waiter(self.message_sender.clone())
            .inspect_err(|_| self.rollback_seized(&seized))?;
        Ok((tgid, seized, unseized, waiter))
    }

    /// Interrupts a seized thread, or retires one that is exiting, whose exit
    /// status is due.
    fn interrupt_or_retire(&mut self, tid: Pid) -> Result<()> {
        if !self.ptrace.interrupt(tid)? {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            inferior.threads.remove(&tid);
            inferior.retired_threads.insert(tid);
        }
        Ok(())
    }

    /// Seizes every thread `tgid` lists, returning those seized and those
    /// that had finished exiting. A failure releases those seized.
    ///
    /// Threads created while these are seized are found once every seized
    /// thread has stopped; see `seize_untraced_threads`. Listing again now
    /// could not find them all, and would find threads already traced
    /// through their creator's clone event, which cannot be seized.
    fn seize_listed_threads(&self, tgid: Pid) -> Result<(BTreeSet<Pid>, BTreeSet<Pid>)> {
        let mut seized = BTreeSet::new();
        let mut unseized = BTreeSet::new();
        let result = (|| -> Result<()> {
            for tid in self.ptrace.process_threads(tgid)? {
                if self.ptrace.seize(tid, false)? {
                    seized.insert(tid);
                } else {
                    unseized.insert(tid);
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.rollback_seized(&seized);
            return Err(error);
        }
        Ok((seized, unseized))
    }

    pub(super) fn rollback_seized(&self, seized: &BTreeSet<Pid>) {
        for &pid in seized {
            let _ = self.ptrace.interrupt(pid);
        }
        for &pid in seized {
            loop {
                match self.ptrace.wait_status(pid) {
                    Ok(WaitEvent::Stopped(..) | WaitEvent::PtraceEvent(..)) => {
                        let _ = self.ptrace.detach(pid, None);
                        break;
                    }
                    Ok(WaitEvent::Exited(..) | WaitEvent::Signaled(..)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        }
    }

    pub(super) fn handle_initial_stop(&mut self, pid: Pid) -> Result<()> {
        self.ptrace.set_options(pid, true)?;
        self.refresh_thread_names();
        let load_bias = self.executable_load_bias(pid)?;
        self.load_main_image(load_bias)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;

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

    /// The load bias of the executable in `pid`'s process, which fails
    /// unless the process runs the executable.
    fn executable_load_bias(&self, pid: Pid) -> Result<u64> {
        self.ptrace.load_bias(
            pid,
            &self.executable,
            &self.executable_data,
            self.executable_identity,
        )
    }

    /// Places the executable's image at `load_bias` in a freshly executed
    /// process and starts following the dynamic loader, which the kernel
    /// mapped with the program, so breakpoints in libraries resolve as they
    /// load.
    fn load_main_image(&mut self, load_bias: u64) -> Result<()> {
        self.place_main_image(load_bias)?;
        self.refresh_libraries()
    }

    /// Places the executable's image at `load_bias` and plants every
    /// breakpoint in it.
    fn place_main_image(&mut self, load_bias: u64) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.loaded_module = LoadedModule::main(self.module_image.id(), load_bias);
        self.modules
            .get_mut(&crate::ModuleId::new(0))
            .expect("main module is registered")
            .loaded = inferior.loaded_module;
        for breakpoint in &self.breakpoints {
            install_logical_breakpoint(&self.ptrace, inferior, breakpoint)?;
        }
        Ok(())
    }

    pub(super) fn handle_initial_attach_stop(&mut self, pid: Pid) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let thread = inferior.thread_mut(pid)?;
        thread.state = NativeThreadState::Stopped;
        thread.expected = ExpectedStop::None;
        self.finish_barrier_if_ready()
    }

    /// Seizes the threads an attach has not traced yet, and returns whether
    /// the attach now waits for any of them to stop.
    ///
    /// The kernel decides whether a new thread is traced as its clone
    /// begins, so a thread seized inside clone creates an untraced thread,
    /// which can join the thread list after any listing taken meanwhile.
    /// Once every seized thread is stopped none is inside clone, and each
    /// thread they created since was announced by its clone event. Any
    /// other listed thread is untraced, and would die of the first
    /// breakpoint it reached.
    pub(super) fn seize_untraced_threads(&mut self) -> Result<bool> {
        loop {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            let untraced = self
                .ptrace
                .process_threads(inferior.tgid)?
                .into_iter()
                .filter(|tid| {
                    !inferior.threads.contains_key(tid)
                        && !inferior.retired_threads.contains(tid)
                        && !inferior.vanished_threads.contains(tid)
                        && !inferior.unseized_threads.contains(tid)
                        && !inferior.unowned_stops.contains_key(tid)
                })
                .collect::<Vec<_>>();
            if untraced.is_empty() {
                return Ok(false);
            }
            // Untraced threads can keep creating untraced threads.
            self.attach_rescans += 1;
            if self.attach_rescans > MAX_ATTACH_RESCANS {
                return Err(backend_error(LinuxError::AttachThreadsUnstable));
            }
            let mut waiting = false;
            for tid in untraced {
                if !self.ptrace.seize(tid, false)? {
                    inferior.unseized_threads.insert(tid);
                    continue;
                }
                if self.ptrace.interrupt(tid)? {
                    inferior
                        .threads
                        .insert(tid, TraceThread::starting(ExpectedStop::InitialAttach));
                    waiting = true;
                } else {
                    // The thread is exiting; its exit status retires it.
                    inferior.retired_threads.insert(tid);
                }
            }
            if waiting {
                return Ok(true);
            }
        }
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn handle_thread_start(&mut self, pid: Pid) -> Result<()> {
        let exit_kill = self
            .inferior
            .as_ref()
            .is_some_and(|inferior| inferior.origin.owned());
        // A sibling's `exit_group` may already have killed the new thread
        // out of its first stop. Its exit status follows and retires it.
        let vanished = match self.ptrace.set_options(pid, exit_kill) {
            Ok(()) => false,
            Err(error) if is_vanished_tracee(&error) => true,
            Err(error) => return Err(error),
        };
        let arm_failure = if vanished {
            None
        } else {
            self.arm_new_thread(pid)
        };
        if let Some(inferior) = self.inferior.as_mut() {
            let name = self.ptrace.thread_name(inferior.tgid, pid);
            inferior.thread_mut(pid)?.name = name;
        }
        let (process_id, barrier_active, should_resume) = {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            let thread = inferior.thread_mut(pid)?;
            thread.state = if vanished {
                NativeThreadState::Exiting
            } else {
                NativeThreadState::Stopped
            };
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
        let load_bias = self.executable_load_bias(pid)?;
        self.place_main_image(load_bias)?;
        self.clear_attached_debug_registers()
    }

    pub(super) fn handle_ptrace_event(&mut self, pid: Pid, event: i32) -> Result<()> {
        // SIGKILL may have taken the thread out of an event's stop since it
        // was reported, and the event message then describes the exit.
        if event != libc::PTRACE_EVENT_EXIT && is_superseded(&self.ptrace.signal_metadata(pid)) {
            return self.handle_classified_stop(pid, ClassifiedStop::Superseded);
        }
        let thread = self
            .inferior
            .as_mut()
            .ok_or(Error::NotRunning)?
            .thread_mut(pid)?;
        match event {
            libc::PTRACE_EVENT_CLONE => {
                thread.state = NativeThreadState::Stopped;
                self.handle_clone_event(pid)
            }
            libc::PTRACE_EVENT_FORK => {
                thread.state = NativeThreadState::Stopped;
                self.handle_fork_event(pid)
            }
            libc::PTRACE_EVENT_EXEC => {
                thread.state = NativeThreadState::Stopped;
                self.handle_exec_event(pid)
            }
            libc::PTRACE_EVENT_EXIT => {
                thread.state = NativeThreadState::Exiting;
                let alone = self.leader_exiting_alone(pid)?;
                self.adopt_unannounced_fork_children(pid);
                self.release_exiting_thread(pid)?;
                if let Some(status) = alone {
                    let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
                    let awaited = inferior.thread(pid)?.awaiting_breakpoint.is_some();
                    return self.thread_exited(pid, status, awaited);
                }
                // A leader exiting alone gives a stop in progress nothing
                // more to wait for.
                self.finish_barrier_if_ready()
            }
            other => self.begin_visible_stop(
                pid,
                StopReason::Unclassifiable {
                    description: format!("unsupported ptrace event {other}").into(),
                },
            ),
        }
    }

    /// The status of a group leader stopped at its exit event that exits
    /// alone, by `exit` while other threads live, or `None` for any other
    /// thread or exit. Linux reports such a leader's exit status only once
    /// every other thread has exited, and the process's own status then
    /// replaces it, so this is the code the thread passed to `exit`, which
    /// its exit event carries.
    fn leader_exiting_alone(&self, pid: Pid) -> Result<Option<ExitStatus>> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        // With every other thread exiting too, the process is ending.
        let others_live = inferior.threads.iter().any(|(&other, thread)| {
            other != pid && !matches!(thread.state, NativeThreadState::Exiting)
        });
        if pid != inferior.tgid || !others_live {
            return Ok(None);
        }
        let (registers, message) = match self
            .ptrace
            .registers(pid)
            .and_then(|registers| Ok((registers, self.ptrace.event_message(pid)?)))
        {
            Err(error) if is_vanished_tracee(&error) => return Ok(None),
            result => result?,
        };
        // The exit stop is inside the system call that began the exit. A
        // status in wait's form with a signal in its low bits is SIGKILL's.
        let Ok(word) = i32::try_from(message) else {
            return Ok(None);
        };
        if registers.orig_rax.cast_signed() != libc::SYS_exit || word & 0x7f != 0 {
            return Ok(None);
        }
        Ok(Some(ExitStatus::Code(i64::from((word >> 8) & 0xff))))
    }

    /// Lets a thread stopped at its exit event finish exiting. A sibling's
    /// `exit_group` may already have released it, which is no failure: its
    /// exit status follows either way.
    pub(super) fn release_exiting_thread(&self, pid: Pid) -> Result<()> {
        match self.ptrace.continue_execution(pid, None) {
            Err(error) if is_vanished_tracee(&error) => Ok(()),
            result => result,
        }
    }

    /// The identifier the event stop of `pid` reports, or `None` once SIGKILL
    /// took the thread out of that stop, even while it was read. A thread
    /// leaves an event's stop only for its exit, whose message replaces the
    /// event's, so the stop is checked after the read.
    fn event_message_of(&self, pid: Pid) -> Result<Option<Pid>> {
        let message = match self.ptrace.event_message(pid) {
            Ok(message) => message,
            Err(error) if is_vanished_tracee(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        if is_superseded(&self.ptrace.signal_metadata(pid)) {
            return Ok(None);
        }
        let message = i32::try_from(message).map_err(|_| Error::AddressOverflow)?;
        Ok(Some(Pid::from_raw(message)))
    }

    pub(super) fn handle_clone_event(&mut self, parent: Pid) -> Result<()> {
        let Some(child) = self.event_message_of(parent)? else {
            return self.handle_classified_stop(parent, ClassifiedStop::Superseded);
        };
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        // A sibling's exit_group can end the new thread before this event
        // announces it.
        let ended = inferior.vanished_threads.remove(&child);
        let child_tgid = if ended {
            None
        } else {
            match self.ptrace.thread_group_id(child) {
                Ok(tgid) => Some(tgid),
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    // The thread is already gone; its exit status follows.
                    inferior.retired_threads.insert(child);
                    None
                }
                Err(error) => return Err(error),
            }
        };
        let Some(child_tgid) = child_tgid else {
            inferior.unowned_stops.remove(&child);
            return self.restart_after_internal(parent);
        };
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
        // The new thread, still starting, keeps a barrier from completing.
        self.restart_after_internal(parent)?;
        if let Some(status) = pending {
            self.process_wait(status)?;
        }
        Ok(())
    }

    /// Releases a forked child, which the debugger does not follow, and
    /// resumes the parent.
    pub(super) fn handle_fork_event(&mut self, parent: Pid) -> Result<()> {
        let Some(child) = self.event_message_of(parent)? else {
            return self.handle_classified_stop(parent, ClassifiedStop::Superseded);
        };
        // The child's memory is the parent's as it forked; later edits, such
        // as lifting a site to step over it, never reach the child.
        let sites = self
            .inferior
            .as_ref()
            .ok_or(Error::NotRunning)?
            .inherited_sites();
        self.track_fork_child(child, sites);
        self.restart_after_internal(parent)
    }

    /// Releases a fork child whose initial stop already arrived, or records
    /// it to be released at that stop.
    fn track_fork_child(&mut self, child: Pid, sites: Vec<(VirtualAddress, u8)>) {
        let Some(inferior) = self.inferior.as_mut() else {
            return;
        };
        if inferior.unowned_stops.remove(&child).is_some() {
            self.release_fork_child(child, &sites);
        } else {
            inferior.fork_children.insert(child, sites);
        }
    }

    /// Detaches a stopped fork child after removing the breakpoints it
    /// inherited, `sites`, which would otherwise kill it with SIGTRAP once
    /// untraced. Debug registers are not inherited across fork. A child that
    /// cannot be cleaned is killed rather than released with traps in place.
    pub(super) fn release_fork_child(&mut self, child: Pid, sites: &[(VirtualAddress, u8)]) {
        if !self.scrub_and_release(child, sites)
            && let Some(inferior) = self.inferior.as_mut()
        {
            inferior.killed_children.insert(child);
        }
    }

    /// Restores the bytes the traps in `sites` replaced in a stopped fork
    /// child and detaches it. A child that cannot be scrubbed is killed
    /// rather than released with traps in place. Returns whether it was
    /// released; one that was not stays traced until it exits.
    fn scrub_and_release(&self, child: Pid, sites: &[(VirtualAddress, u8)]) -> bool {
        let mut cleaned = Ok(());
        for &(address, original_byte) in sites {
            let word = match self.ptrace.read_memory_word(child, address.get()) {
                Ok(word) => word,
                // Memory the child cannot read, such as a library unloaded
                // since, holds no trap it could execute.
                Err(MemoryAccessError::Inaccessible) => continue,
                Err(MemoryAccessError::Fatal(error)) => {
                    cleaned = Err(error);
                    break;
                }
                // Only a core dump reads part of a word.
                Err(MemoryAccessError::Partial { .. }) => {
                    cleaned = Err(backend_error(LinuxError::System(Errno::EIO)));
                    break;
                }
            };
            let mut bytes = word.to_ne_bytes();
            // Only a trap is ours to remove. A site removed before the fork,
            // or whose memory was remapped since, such as an unloaded
            // library's, holds code the child must keep.
            if bytes[0] != BREAKPOINT_OPCODE {
                continue;
            }
            bytes[0] = original_byte;
            cleaned = self
                .ptrace
                .write_word(child, address.get(), u64::from_ne_bytes(bytes));
            if cleaned.is_err() {
                break;
            }
        }
        cleaned
            .and_then(|()| self.ptrace.detach(child, None))
            .unwrap_or_else(|_| {
                let _ = self.ptrace.kill(child, Signal::SIGKILL);
                false
            })
    }

    /// Finds the children `thread` forked whose fork events SIGKILL took
    /// away, while they are still its children, so they are scrubbed and
    /// released like any other.
    fn adopt_unannounced_fork_children(&mut self, thread: Pid) {
        let Some(inferior) = self.inferior.as_ref() else {
            return;
        };
        let sites = inferior.inherited_sites();
        for child in self.ptrace.traced_children(inferior.tgid, thread) {
            let Some(inferior) = self.inferior.as_ref() else {
                return;
            };
            if !inferior.fork_children.contains_key(&child)
                && !inferior.killed_children.contains(&child)
                && !inferior.threads.contains_key(&child)
            {
                self.track_fork_child(child, sites.clone());
            }
        }
    }

    /// Handles a status of a fork child that outlived the inferior.
    pub(super) fn handle_orphan_wait(&mut self, status: WaitEvent) -> bool {
        let pid = status.pid();
        let orphans = self.orphans.as_mut().expect("orphans exist");
        match status {
            WaitEvent::Stopped(..) | WaitEvent::PtraceEvent(_, _, libc::PTRACE_EVENT_STOP) => {
                let sites = orphans.children.remove(&pid);
                if let Some(sites) = sites
                    && !self.scrub_and_release(pid, &sites)
                {
                    self.orphans
                        .as_mut()
                        .expect("orphans exist")
                        .killed
                        .insert(pid);
                }
            }
            WaitEvent::PtraceEvent(_, _, libc::PTRACE_EVENT_EXIT) => {
                let _ = self.release_exiting_thread(pid);
            }
            WaitEvent::Exited(..) | WaitEvent::Signaled(..) => {
                orphans.children.remove(&pid);
                orphans.killed.remove(&pid);
            }
            _ => {}
        }
        let orphans = self.orphans.as_ref().expect("orphans exist");
        if !orphans.children.is_empty() || !orphans.killed.is_empty() {
            return true;
        }
        let orphans = self.orphans.take().expect("orphans exist");
        // The program is gone, so a failure has no one to report to.
        if orphans.waiter.is_some_and(|waiter| waiter.join().is_err()) {
            record!("the waiter of the orphaned children panicked");
        }
        if self.shutting_down {
            self.finish_shutdown(Ok(()));
            return false;
        }
        if let Some(start) = self.deferred_start.take() {
            self.start(start);
        }
        true
    }

    pub(super) fn handle_exec_event(&mut self, pid: Pid) -> Result<()> {
        let Some(old_tid) = self.event_message_of(pid)? else {
            return self.handle_classified_stop(pid, ClassifiedStop::Superseded);
        };
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let mut survivor = inferior
            .threads
            .remove(&old_tid)
            .or_else(|| inferior.threads.remove(&pid))
            .unwrap_or_else(|| TraceThread::starting(ExpectedStop::None));
        if matches!(survivor.expected, ExpectedStop::AdoptedExec) {
            survivor.expected = ExpectedStop::InitialExec;
            inferior.threads.insert(pid, survivor);
            if self.executable_load_bias(pid).is_err() {
                return Err(backend_error(LinuxError::ExecutedAnotherProgram(
                    self.executable.to_path_buf(),
                )));
            }
            return self.handle_initial_stop(pid);
        }
        inferior
            .retired_threads
            .extend(inferior.threads.keys().copied());
        inferior.threads.clear();
        survivor.state = NativeThreadState::Stopped;
        survivor.expected = ExpectedStop::None;
        survivor.pending_signal = None;
        survivor.held_signal = None;
        survivor.stopped_at_breakpoint = None;
        survivor.trapped_at = None;
        survivor.awaiting_breakpoint = None;
        survivor.debugger_stop_pending = false;
        // The execing thread takes over the leader's TID, whose exit the
        // kernel never reports.
        inferior.retired_threads.remove(&pid);
        inferior.threads.insert(pid, survivor);
        inferior.breakpoints.clear();
        inferior.former_sites.clear();
        inferior.plan_sites.clear();
        inferior.repairs.clear();
        inferior.loader_site = None;
        inferior.runtime_hooks.clear();
        // exec(2) flushes every debug register; the new image's addresses
        // have no relation to the old watchpoints.
        self.discard_watchpoints();
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let generation = inferior.watch.generation;
        if let Some(thread) = inferior.threads.get_mut(&pid) {
            thread.armed = Some(generation);
            thread.watch_hits.clear();
        }
        self.reset_runtime_modules();
        self.refresh_thread_names();
        // Only the executable's own image can be followed: it is the one
        // whose debug information is loaded.
        let load_bias = self.executable_load_bias(pid);
        let followed = load_bias.is_ok();
        if let Ok(load_bias) = load_bias {
            self.load_main_image(load_bias)?;
        } else {
            self.inferior
                .as_mut()
                .ok_or(Error::NotRunning)?
                .exec_unsupported = true;
        }
        self.begin_visible_stop(pid, StopReason::Exec { followed })
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Collects SIGTRAPs seized threads queued before they were interrupted,
    /// and returns whether any thread was resumed for one.
    ///
    /// `PTRACE_INTERRUPT` stops a thread before it dequeues signals, so a trap
    /// raised just before it is still queued: publishing the stop would report
    /// the trap only after a later resume, and detaching would deliver it to
    /// an untraced process. Resumed without a signal, the thread dequeues the
    /// trap into a signal-delivery stop before running any instruction.
    pub(super) fn drain_queued_traps(&mut self) -> Result<bool> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        if !inferior.origin.seized() {
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
        if pid == inferior.tgid {
            // The kernel reports the leader's exit only once every other
            // thread is gone, even those whose own exits went unseen.
            let others = std::mem::take(&mut inferior.threads);
            inferior.retired_threads.extend(others.into_keys());
        }
        let process_id = process_id(inferior.tgid);
        let execution = inferior.active.as_ref().map(|active| active.id);

        if inferior.threads.is_empty() {
            self.discard_watchpoints();
            let pending = self.release_stopped_fork_children();
            let mut inferior = self.inferior.take().expect("inferior exists");
            let edits = inferior.take_pending_edits();
            let killed = std::mem::take(&mut inferior.killed_children);
            let waiter = inferior.waiter.take();
            if pending.is_empty() && killed.is_empty() {
                if let Some(waiter) = waiter {
                    waiter.join()?;
                }
            } else {
                // The waiter reports the children's first stops, at which
                // they are released, and the exits of those killed.
                self.orphans = Some(Orphans {
                    children: pending,
                    killed,
                    waiter,
                });
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
            if let Some(reply) = self.kill_reply.take() {
                let _ = reply.send(Ok(()));
            }
            self.settle_pending_edits(edits);
            return Ok(());
        }

        self.thread_exited(pid, status, exited.awaiting_breakpoint.is_some())
    }

    /// Publishes the exit of a thread whose process lives on, and ends or
    /// advances the execution it affects: one of the thread alone ends in
    /// the exit. `awaited_breakpoint` says the thread was resumed alone to
    /// reach a breakpoint it awaited.
    fn thread_exited(
        &mut self,
        pid: Pid,
        status: ExitStatus,
        awaited_breakpoint: bool,
    ) -> Result<()> {
        let thread_id = debug_thread_id(pid);
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let process_id = process_id(inferior.tgid);
        let owned_execution = inferior.active.as_ref().is_some_and(|active| {
            matches!(active.scope, ResumeScope::Thread(thread) if thread == thread_id)
                || matches!(active.kind, ActiveKind::Step { owner, .. } if owner.thread == pid)
        });
        // A thread resumed alone to reach its awaited breakpoint holds its
        // siblings back; they must run once it is gone.
        let ran_alone = awaited_breakpoint && inferior.active.is_some();
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
            {
                // The execution has no thread left to resume, so the stop
                // ends in this exit unless it publishes something that
                // matters more, and even if an edit drops that.
                let reason = StopReason::ThreadExited { thread_id, status };
                barrier.ended = Some(reason.clone());
                barrier.reason.get_or_insert(reason);
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
            let running = inferior
                .threads
                .iter()
                .find(|&(&pid, thread)| !inferior.exited_leader(pid, thread))
                .map(|(&pid, _)| pid)
                .ok_or(Error::NotRunning)?;
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
        if let Some(start) = self.deferred_start.take() {
            start.fail(Error::RequestCancelled);
        }
        self.launch_reply
            .take()
            .map(|reply| reply.send(Err(Error::RequestCancelled)));
        self.attach_reply
            .take()
            .map(|reply| reply.send(Err(Error::RequestCancelled)));

        let Some(inferior) = self.inferior.as_ref() else {
            // Children the ended inferior forked are released first.
            if self.orphans.is_none() {
                self.finish_shutdown(Ok(()));
            }
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
            if let Err(error) = self.interrupt_or_retire(tid) {
                self.finish_shutdown(Err(error));
                return;
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
                // A thread no event announced yet exited; one may still come.
                // Every record of the pid ends with it.
                let retired = inferior.retired_threads.remove(&pid);
                let killed = inferior.killed_children.remove(&pid);
                let forked = inferior.fork_children.remove(&pid).is_some();
                if !(retired || killed || forked) {
                    inferior.vanished_threads.insert(pid);
                }
                inferior.unowned_stops.remove(&pid);
            }
            WaitEvent::PtraceEvent(_, _, libc::PTRACE_EVENT_EXIT) => {
                // A thread no event announced yet is exiting; let it go.
                inferior.unowned_stops.remove(&pid);
                inferior.vanished_threads.insert(pid);
                return self.release_exiting_thread(pid).is_ok();
            }
            WaitEvent::Stopped(..) | WaitEvent::PtraceEvent(_, _, libc::PTRACE_EVENT_STOP) => {
                if let Some(sites) = inferior.fork_children.remove(&pid) {
                    self.release_fork_child(pid, &sites);
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
    fn release_stopped_fork_children(&mut self) -> BTreeMap<Pid, Vec<(VirtualAddress, u8)>> {
        let Some(inferior) = self.inferior.as_mut() else {
            return BTreeMap::new();
        };
        let stopped = std::mem::take(&mut inferior.unowned_stops);
        let pending = std::mem::take(&mut inferior.fork_children);
        let sites = inferior.inherited_sites();
        for &pid in stopped.keys() {
            self.release_fork_child(pid, &sites);
        }
        pending
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
            return self.inferior.is_some() || self.orphans.is_some();
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
                self.adopt_unannounced_fork_children(pid);
                self.ptrace.continue_during_shutdown(pid)
            }
            // SIGKILL may have moved the thread on to its exit stop since
            // this status, so continuing it may let it exit: the children it
            // forked are found while `/proc` still lists them.
            WaitEvent::Stopped(pid, _) | WaitEvent::PtraceEvent(pid, _, _) => {
                self.adopt_unannounced_fork_children(pid);
                self.kill_inferior()
                    .and_then(|()| self.ptrace.continue_during_shutdown(pid))
            }
            other => Err(backend_error(LinuxError::UnexpectedWait(format!(
                "{other:?}"
            )))),
        };
        if let Err(error) = result {
            self.finish_shutdown(Err(error));
            return false;
        }
        self.inferior.is_some() || self.orphans.is_some()
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
                self.adopt_unannounced_fork_children(pid);
                // An exiting leader settles the detach: Linux reports its
                // exit only after every other thread's.
                if let Some(thread) = self
                    .inferior
                    .as_mut()
                    .and_then(|inferior| inferior.threads.get_mut(&pid))
                {
                    thread.state = NativeThreadState::Exiting;
                }
                self.release_exiting_thread(pid)
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
        self.inferior.is_some() || self.orphans.is_some()
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
                .iter()
                .all(|(&pid, thread)| inferior.settled(pid, thread));
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
        self.settle_pending_edits(inferior.take_pending_edits());
        self.finish_shutdown(outcome);
    }

    /// Records a fork child announced while detaching; its initial stop
    /// releases it, and the detach waits for that.
    fn handle_fork_during_detach(&mut self, parent: Pid) -> Result<()> {
        let event = self.event_message_of(parent)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        // The killed parent goes on to its exit, which the detach awaits.
        let Some(child) = event else {
            inferior.thread_mut(parent)?.state = NativeThreadState::Running;
            return Ok(());
        };
        inferior.thread_mut(parent)?.state = NativeThreadState::Stopped;
        let sites = inferior.inherited_sites();
        self.track_fork_child(child, sites);
        Ok(())
    }

    pub(super) fn handle_clone_during_detach(&mut self, parent: Pid) -> Result<()> {
        let event = self.event_message_of(parent)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        // The killed parent goes on to its exit, which the detach awaits.
        let Some(child) = event else {
            inferior.thread_mut(parent)?.state = NativeThreadState::Running;
            return Ok(());
        };
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
        // Every thread was stopped, and only SIGKILL takes one out of its
        // stop: a thread that refuses to be written belongs to a process
        // that is ending, where what is left can never run.
        let mut first_error = self
            .disarm_for_detach()
            .err()
            .filter(|error| !is_vanished_tracee(error));
        let mut record = |result: Result<()>| {
            if let Err(error) = result
                && !is_vanished_tracee(&error)
            {
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
                .or(thread.held_signal)
                .map(|pending| pending.signal)
                .filter(|signal| self.signals.get(*signal).pass);
            record(self.ptrace.detach(pid, signal).map(drop));
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
        self.settle_pending_edits(inferior.take_pending_edits());
        self.finish_shutdown(first_error.map_or(Ok(()), Err));
    }

    /// Kills the inferior and replies once its exit is processed.
    pub(super) fn kill(&mut self, reply: Reply<()>) {
        if self.inferior.is_none() {
            let _ = reply.send(Err(Error::NotRunning));
            return;
        }
        if self.kill_reply.is_some() {
            let _ = reply.send(Err(Error::RequestCancelled));
            return;
        }
        match self.kill_inferior() {
            Ok(()) => self.kill_reply = Some(reply),
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    /// Sends the inferior `SIGTERM`, which is delivered without stopping,
    /// and resumes a stopped inferior so it receives it.
    pub(super) fn terminate(&mut self) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let signal = Signal::SIGTERM;
        inferior.terminating = Some(Terminating {
            signal,
            delivered: false,
        });
        let (tgid, stop) = (
            inferior.tgid,
            inferior.public_stop.as_ref().map(|stop| stop.id),
        );
        self.ptrace.kill(tgid, signal)?;
        let Some(stop) = stop else {
            return Ok(());
        };
        let process = process_id(tgid);
        let scope = ResumeScope::Process(process);
        let execution = self.begin_execution(
            process,
            stop,
            scope,
            ActiveKind::Continue,
            ExceptionDisposition::Pass,
        )?;
        let _ = self.events.send(DebuggerEvent::InferiorContinued {
            revision: self.revision,
            process_id: process,
            execution_id: execution,
            resumed: scope,
        });
        Ok(())
    }

    pub(super) fn kill_inferior(&self) -> Result<()> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(());
        };
        self.ptrace.kill(inferior.tgid, Signal::SIGKILL)
    }

    pub(super) fn fail_inferior(&mut self, error: Error) {
        record!("inferior failed: {error}");
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

impl Start {
    /// Answers the start's client with `error`.
    fn fail(self, error: Error) {
        match self {
            Self::Launch(_, reply) | Self::LaunchByExec { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Attach(_, reply) => {
                let _ = reply.send(Err(error));
            }
        }
    }
}

/// The native identifier of a requested process.
fn requested_pid(requested: ProcessId) -> Result<Pid> {
    i32::try_from(requested.get())
        .ok()
        .filter(|raw| *raw > 0)
        .map(Pid::from_raw)
        .ok_or(Error::InvalidProcessId(requested.get()))
}
