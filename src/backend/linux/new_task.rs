//! Stepping into the task a line starts.
//!
//! The step runs as a step over its line while it watches the entry of
//! each runtime's task starter. When the step's own task enters one, the
//! step waits for the starter's return on that thread, where the runtime
//! names the task it made and where the task begins. The step then belongs
//! to the new task: it waits at that entry for the task to run, and goes
//! on from there as a step in, through wrappers, to the first statement of
//! the program's own code. A line that starts no task ends as a step over.

use std::collections::{BTreeMap, BTreeSet};

use nix::unistd::Pid;

use crate::protocol::{StepKind, StopReason};
use crate::{Error, Result, RuntimeId, TaskId, VirtualAddress};

use super::breakpoints::install_plan_breakpoint;
use super::native::{InspectionOps, LinuxTraceOps};
use super::registers::x86_64_registers;
use super::{ActiveKind, Controller, StepOwner, StepStart};

/// How far a step into a new task has followed the task's start.
#[derive(Debug, Clone)]
pub(super) enum NewTask {
    /// Watching for the step's task to enter a runtime's task starter, at
    /// these entries.
    Watching(BTreeMap<VirtualAddress, RuntimeId>),
    /// The step's task entered the starter of `runtime` on `thread`, which
    /// returns to `returns_to` with the stack pointer at `stack`.
    Starting {
        starters: BTreeMap<VirtualAddress, RuntimeId>,
        runtime: RuntimeId,
        thread: Pid,
        returns_to: VirtualAddress,
        stack: u64,
    },
    /// The task started, and the step waits for it where it begins.
    Started { entry: VirtualAddress },
    /// The task began, and the step goes on in it as a step in.
    Entered,
}

impl<P: LinuxTraceOps> Controller<P> {
    /// The entries of the task starters of every runtime loaded, which a
    /// step into a new task watches.
    pub(super) fn task_starters(&self) -> BTreeMap<VirtualAddress, RuntimeId> {
        let Some(inferior) = self.inferior.as_ref() else {
            return BTreeMap::new();
        };
        self.runtimes(inferior)
            .into_iter()
            .filter_map(|runtime| {
                let entry = runtime.model.task_starter()?;
                Some((runtime.module.virtual_address(entry).ok()?, runtime.id))
            })
            .collect()
    }

    /// Follows a step into a new task at one of its plan's breakpoints,
    /// which the step's task reached. Returns whether the site was one of
    /// the task's start and has been handled.
    pub(super) fn reach_new_task(&mut self, pid: Pid, address: VirtualAddress) -> Result<bool> {
        let Some(state) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { start, .. } => start.new_task.clone(),
                _ => None,
            })
        else {
            return Ok(false);
        };
        match state {
            NewTask::Watching(starters) | NewTask::Starting { starters, .. }
                if starters.contains_key(&address) =>
            {
                self.enter_starter(pid, address, starters)?;
            }
            NewTask::Starting {
                runtime,
                thread,
                returns_to,
                stack,
                ..
            } if returns_to == address => {
                // The return of another call, as of a starter the task's
                // handler for a signal entered, is not the start's.
                if thread == pid && self.ptrace.registers(pid)?.rsp == stack {
                    self.return_from_starter(pid, address, runtime)?;
                } else {
                    self.repair_when_alone(pid, address)?;
                }
            }
            NewTask::Started { entry } if entry == address => self.enter_new_task(pid, address)?,
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Waits for the return of the task starter the step's task entered.
    fn enter_starter(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
        starters: BTreeMap<VirtualAddress, RuntimeId>,
    ) -> Result<()> {
        let registers = self.ptrace.registers(pid)?;
        let returns_to = VirtualAddress::new(self.ptrace.read_word(pid, registers.rsp)?);
        record!("thread {pid} starts a task, returning to {returns_to}");
        let execution = self.active_execution()?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        install_plan_breakpoint(&self.ptrace, inferior, returns_to, execution)?;
        let runtime = starters[&address];
        if let Some(start) = self.active_step_mut() {
            start.new_task = Some(NewTask::Starting {
                starters,
                runtime,
                thread: pid,
                returns_to,
                stack: registers.rsp.wrapping_add(8),
            });
        }
        self.repair_when_alone(pid, address)
    }

    /// Moves the step to the task the starter returned, and waits for the
    /// task where it begins.
    fn return_from_starter(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
        runtime: RuntimeId,
    ) -> Result<()> {
        let registers = self.ptrace.registers(pid)?;
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let started = self
            .runtimes(inferior)
            .into_iter()
            .find(|bound| bound.id == runtime)
            .ok_or_else(|| "its runtime is gone".into())
            .and_then(|bound| {
                self.with_runtime_stop(inferior, &bound, pid, |stop| {
                    bound
                        .model
                        .started_task(stop, &x86_64_registers(&registers))
                })
            });
        let (number, entry) = match started {
            Ok(task) => {
                let Some(entry) = task.entry else {
                    return self.lose_new_task(pid, "the task started begins nowhere".into());
                };
                (task.number, entry)
            }
            Err(reason) => {
                return self.lose_new_task(pid, format!("the task started is unknown: {reason}"));
            }
        };
        let task = TaskId { runtime, number };
        record!("the step goes on in task {task}, which begins at {entry}");
        let execution = self.active_execution()?;
        self.cleanup_plan_breakpoints(execution)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        install_plan_breakpoint(&self.ptrace, inferior, entry, execution)?;
        if let Some(ActiveKind::Step { owner, start, .. }) =
            inferior.active.as_mut().map(|active| &mut active.kind)
        {
            *owner = StepOwner {
                thread: pid,
                task: Some(task),
            };
            **start = StepStart {
                plan_addresses: BTreeSet::from([entry]),
                new_task: Some(NewTask::Started { entry }),
                ..StepStart::default()
            };
        }
        self.go_on_without_plan(pid, address, None)
    }

    /// Goes on as a step in from where the new task begins, as if the
    /// task had just called its first function.
    fn enter_new_task(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        let registers = self.ptrace.registers(pid)?;
        let position = self.stack_position(pid, &registers);
        let activation = self.top_activation(pid, &registers).ok();
        record!("the step's new task begins on thread {pid}");
        let execution = self.active_execution()?;
        self.cleanup_plan_breakpoints(execution)?;
        if let Some(start) = self.active_step_mut() {
            *start = StepStart {
                activation,
                stack_pointer: Some(position),
                following: true,
                new_task: Some(NewTask::Entered),
                ..StepStart::default()
            };
        }
        let kind = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { kind, .. } => Some(*kind),
                _ => None,
            })
            .ok_or(Error::NotRunning)?;
        self.go_on_without_plan(pid, address, Some(kind))
    }

    /// The stop a step reports for `reason`, naming the kind of step the
    /// client asked for when the step ran as another kind. Any other stop,
    /// such as an advance reaching its location, names itself.
    pub(super) fn requested_step_reason(&self, reason: StopReason) -> StopReason {
        let Some((running, requested)) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step {
                    kind, requested, ..
                } => Some((*kind, *requested)),
                _ => None,
            })
        else {
            return reason;
        };
        match reason {
            StopReason::Step { kind } if kind == running => StopReason::Step { kind: requested },
            StopReason::StepIncomplete { kind, description } if kind == running => {
                StopReason::StepIncomplete {
                    kind: requested,
                    description,
                }
            }
            StopReason::TaskEnded { kind, task, ending } if kind == running => {
                StopReason::TaskEnded {
                    kind: requested,
                    task,
                    ending,
                }
            }
            reason => reason,
        }
    }

    /// Ends a step into a new task that cannot follow it, where the step's
    /// task is.
    fn lose_new_task(&mut self, pid: Pid, description: String) -> Result<()> {
        record!("the step lost its new task: {description}");
        self.begin_visible_stop(
            pid,
            StopReason::StepIncomplete {
                kind: StepKind::IntoNewTask,
                description: description.into(),
            },
        )
    }
}

impl<P: InspectionOps> Controller<P> {
    /// Whether the active step went on into the new task it started,
    /// whose stop then reveals inline frames as a step in's does.
    pub(super) fn entered_new_task(&self) -> bool {
        self.inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .is_some_and(|active| {
                matches!(
                    &active.kind,
                    ActiveKind::Step { start, .. }
                        if matches!(start.new_task, Some(NewTask::Entered))
                )
            })
    }
}
