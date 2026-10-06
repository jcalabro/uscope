//! Internal all-stops: stopping every thread without publishing a stop, so
//! breakpoints and watchpoints can be edited, and breakpoint sites repaired,
//! while the inferior runs.
//!
//! An internal stop is a [`StopBarrier`] without a reason. Once every thread
//! is stopped its edits apply, and execution resumes under the same
//! [`ExecutionId`](crate::ExecutionId) without any stop or continue event.
//! A thread that stops for a visible reason meanwhile, such as a breakpoint
//! or a signal, gives the barrier that reason, and the stop is published as
//! usual once the edits apply.

use nix::unistd::Pid;

use crate::protocol::StopReason;
use crate::{Error, VirtualAddress};

use super::classify::visible_stop_priority;
use super::native::LinuxTraceOps;
use super::run_control::collect_repairs;
use super::{ActiveKind, Controller, Edit, Inferior, NativeThreadState, Result, StopBarrier};

impl<P: LinuxTraceOps> Controller<P> {
    /// Applies a breakpoint or watchpoint edit once every thread is stopped.
    ///
    /// At a published stop, before launch, and while a launch or attach is
    /// still establishing its first stop, the edit applies at once. While
    /// the inferior runs, every thread is stopped first.
    pub(super) fn edit(&mut self, edit: Edit) {
        if self.shutting_down {
            edit.reject(Error::RequestCancelled);
            return;
        }
        let running = self
            .inferior
            .as_ref()
            .is_some_and(|inferior| inferior.public_stop.is_none())
            && self.launch_reply.is_none()
            && self.attach_reply.is_none();
        if !running {
            self.apply_edit(edit);
            return;
        }
        if let Err(error) = self.begin_internal_stop() {
            edit.reject(error);
            return;
        }
        self.inferior
            .as_mut()
            .and_then(|inferior| inferior.barrier.as_mut())
            .expect("an internal stop has a barrier")
            .edits
            .push(edit);
        if let Err(error) = self.finish_barrier_if_ready() {
            self.fail_inferior(error);
        }
    }

    /// Answers the edits an internal stop still held when its process
    /// ended. They change the logical breakpoints and watchpoints, which
    /// outlive any process, unless the session is ending.
    pub(super) fn settle_pending_edits(&mut self, edits: Vec<Edit>) {
        for edit in edits {
            if self.shutting_down {
                edit.reject(Error::RequestCancelled);
            } else {
                self.apply_edit(edit);
            }
        }
    }

    /// Whether breakpoint traps and debug registers can be written now:
    /// the inferior has completed its first stop and every thread is
    /// settled.
    pub(super) fn sites_live(&self) -> bool {
        self.launch_reply.is_none()
            && self.attach_reply.is_none()
            && self.inferior.as_ref().is_some_and(|inferior| {
                inferior
                    .threads
                    .iter()
                    .all(|(&pid, thread)| inferior.settled(pid, thread))
            })
    }

    pub(super) fn apply_edit(&mut self, edit: Edit) {
        match edit {
            Edit::AddBreakpoint {
                spec,
                options,
                reply,
            } => {
                let _ = reply.send(self.add_breakpoint(spec, *options));
            }
            Edit::RemoveBreakpoint { id, reply } => {
                let _ = reply.send(self.remove_breakpoint(id));
            }
            Edit::RemoveAllBreakpoints { reply } => {
                let _ = reply.send(self.remove_all_breakpoints());
            }
            Edit::AddWatchpoint {
                spec,
                access,
                reply,
            } => {
                let _ = reply.send(self.add_watchpoint(spec, access));
            }
            Edit::RemoveWatchpoint { id, reply } => {
                let _ = reply.send(self.remove_watchpoint(id));
            }
            Edit::RemoveAllWatchpoints { reply } => {
                let _ = reply.send(self.remove_all_watchpoints());
            }
            Edit::RefreshModules => {
                // A failed refresh leaves the previous modules; the next stop
                // refreshes them again.
                let _ = self.refresh_libraries();
            }
        }
    }

    /// Starts stopping every running thread without a reason to publish,
    /// unless a barrier is already doing so. The caller completes it with
    /// [`Self::finish_barrier_if_ready`].
    pub(super) fn begin_internal_stop(&mut self) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        if inferior.barrier.is_some() {
            return Ok(());
        }
        let triggering_thread = *inferior.threads.keys().next().ok_or(Error::NotRunning)?;
        inferior.barrier = Some(StopBarrier {
            triggering_thread,
            reason: None,
            paused: false,
            ended: None,
            edits: Vec::new(),
        });
        self.request_stops()
    }

    /// Resumes the active execution from a completed internal stop: steps
    /// every thread stopped at a breakpoint over it, then resumes the
    /// execution's threads as it had them running.
    pub(super) fn resume_after_internal_stop(&mut self) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.barrier = None;
        if inferior.active.is_none() {
            return Ok(());
        }
        inferior.repairs = collect_repairs(inferior);
        self.sync_debug_registers()?;
        self.advance_execution()
    }

    /// Steps a thread over the breakpoint site it stopped at, which no
    /// published stop reports. Removing the site lets any other running
    /// thread pass it unnoticed, so every other thread is stopped first.
    pub(super) fn repair_when_alone(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.thread_mut(pid)?.stopped_at_breakpoint = Some(address);
        if inferior.barrier.is_none() && others_stopped(inferior, pid) {
            self.queue_repair(pid, address);
            return self.start_next_repair();
        }
        // The repair runs when the stop resumes execution.
        self.begin_internal_stop()?;
        self.finish_barrier_if_ready()
    }

    /// Records that the stepping thread executed an instruction whose
    /// effect on the step is evaluated before the step resumes.
    pub(super) fn note_step_progress(&mut self, pid: Pid) {
        if let Some(ActiveKind::Step {
            thread,
            progress_owed,
            ..
        }) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.active.as_mut())
            .map(|active| &mut active.kind)
            && *thread == pid
        {
            *progress_owed = true;
        }
    }

    /// Resumes every stopped thread of the active execution's scope, unless
    /// a barrier is stopping them or the execution has ended.
    pub(super) fn continue_scope_threads(&mut self) -> Result<()> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(());
        };
        if inferior.barrier.is_some() {
            return Ok(());
        }
        let Some(active) = inferior.active.as_ref() else {
            return Ok(());
        };
        let threads = active
            .resume_threads
            .iter()
            .copied()
            .filter(|pid| {
                inferior
                    .threads
                    .get(pid)
                    .is_some_and(|thread| matches!(thread.state, NativeThreadState::Stopped))
            })
            .collect::<Vec<_>>();
        for pid in threads {
            self.continue_thread(pid)?;
        }
        Ok(())
    }

    /// Reconciles the threads' stop reasons with the edits the barrier just
    /// applied and the watched bytes as they now are.
    ///
    /// As with gdb's moribund locations, a hit whose breakpoint or watchpoint
    /// was removed while it was reported is dropped, and so is a change
    /// watchpoint's hit once another thread restored the bytes. The barrier
    /// then publishes the next most important reason, or turns internal.
    pub(super) fn settle_edited_reasons(&mut self) -> Result<()> {
        let unchanged = self.unchanged_watchpoints(
            self.inferior
                .iter()
                .flat_map(|inferior| inferior.threads.values())
                .flat_map(|thread| thread.watch_hits.iter().copied()),
        )?;
        // A stop holds a hit or two, so looking each up beats indexing every
        // breakpoint at every stop.
        let breakpoints = &self.breakpoints;
        let exists = |id| breakpoints.iter().any(|breakpoint| breakpoint.id == id);
        let Some(inferior) = self.inferior.as_mut() else {
            return Ok(());
        };
        let Some(resumed) = inferior
            .active
            .as_ref()
            .map(|active| active.resume_threads.clone())
        else {
            return Ok(());
        };
        for (pid, thread) in &mut inferior.threads {
            if !resumed.contains(pid) {
                continue;
            }
            thread.watch_hits.retain(|id| {
                inferior.watch.watchpoints.contains_key(id) && !unchanged.contains(id)
            });
            match &thread.reason {
                Some(StopReason::Breakpoint { address, hits }) => {
                    let hits = hits
                        .iter()
                        .copied()
                        .filter(|hit| exists(hit.breakpoint))
                        .collect::<Vec<_>>();
                    let address = *address;
                    thread.reason = (!hits.is_empty()).then(|| StopReason::Breakpoint {
                        address,
                        hits: hits.into(),
                    });
                }
                Some(StopReason::Watchpoint { .. }) if thread.watch_hits.is_empty() => {
                    thread.reason = None;
                }
                _ => {}
            }
        }
        let Some(barrier) = inferior.barrier.as_mut() else {
            return Ok(());
        };
        if !matches!(
            barrier.reason,
            Some(StopReason::Breakpoint { .. } | StopReason::Watchpoint { .. })
        ) {
            return Ok(());
        }
        // A barrier publishes its triggering thread's own reason.
        barrier.reason = inferior
            .threads
            .get(&barrier.triggering_thread)
            .and_then(|thread| thread.reason.clone());
        if barrier.reason.is_some() {
            return Ok(());
        }
        // The published hit is gone; another thread's reason, if any, takes
        // its place, then the end of an execution that cannot resume, and a
        // pause the hit outranked otherwise.
        if let Some((pid, reason)) = inferior
            .threads
            .iter()
            .filter(|(pid, _)| resumed.contains(pid))
            .filter_map(|(&pid, thread)| thread.reason.clone().map(|reason| (pid, reason)))
            .max_by_key(|(_, reason)| visible_stop_priority(reason))
        {
            barrier.triggering_thread = pid;
            barrier.reason = Some(reason);
        } else if let Some(reason) = barrier.ended.clone() {
            barrier.reason = Some(reason);
        } else if barrier.paused {
            barrier.reason = Some(StopReason::Pause);
        }
        Ok(())
    }
}

impl Edit {
    /// Answers the edit's client with `error` without applying it.
    fn reject(self, error: Error) {
        match self {
            Self::AddBreakpoint { reply, .. } | Self::RemoveBreakpoint { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::RemoveAllBreakpoints { reply } => {
                let _ = reply.send(Err(error));
            }
            Self::AddWatchpoint { reply, .. } | Self::RemoveWatchpoint { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::RemoveAllWatchpoints { reply } => {
                let _ = reply.send(Err(error));
            }
            Self::RefreshModules => {}
        }
    }
}

/// Whether every thread except `pid` is stopped.
fn others_stopped(inferior: &Inferior, pid: Pid) -> bool {
    inferior
        .threads
        .iter()
        .all(|(&other, thread)| other == pid || inferior.settled(other, thread))
}
