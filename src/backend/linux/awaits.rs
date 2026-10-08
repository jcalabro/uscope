//! Steps through an async function's awaits.
//!
//! An async function's body runs in a function that its future's every
//! poll calls anew. A poll that reaches an await of a future that is not
//! ready returns `Pending` to its caller, with the future's state saying
//! where it waits. A step over or out of such a body does not end in the
//! caller then: it waits for the same future to be polled again, at the
//! point where that state resumes, and goes on from there as the step it
//! was. The future is pinned, so its address names it from one poll to the
//! next, whichever thread polls it; another future of the same function
//! that resumes there is not the step's.
//!
//! A future may be dropped while the step waits for it: its runtime drops
//! a task's future as it cancels the task, and a `select!` or a timeout
//! drops a future it no longer awaits. The step watches the future's drop
//! glue for that. Dropped by its runtime, the future's task was cancelled,
//! and the step ends there; dropped by the program's code, the step goes on
//! in that code to its next line.

use nix::unistd::Pid;

use crate::protocol::{StepKind, StopReason, TaskEnding};
use crate::runtime_model::futures::{self, AsyncFrameKind};
use crate::unwind::DEFAULT_MAX_FRAMES;
use crate::{
    CodeInstanceId, CoroutineStateKind, Error, Result, SourceLocation, TypeReference,
    VirtualAddress,
};

use super::frames::{FrameScope, ResolvedFrame, StackRoot};
use super::native::LinuxTraceOps;
use super::{ActiveKind, Controller, StepStart};
use crate::PresentedFrame;

/// The future whose body a step runs, which the step follows across its
/// polls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AwaitStep {
    future: RunningFuture,
    /// The line the step began on, which it goes on past once the future
    /// resumes.
    source: Option<SourceLocation>,
    /// Where the future resumes, once a poll of it returned `Pending` and
    /// the step waits for the next.
    waiting: Option<VirtualAddress>,
    /// Where the code that drops the future begins, which the step watches
    /// while it waits, when one function drops every future of its type.
    drop_glue: Option<VirtualAddress>,
}

impl AwaitStep {
    /// Whether the step waits for its future to be polled again.
    pub(super) const fn waits(&self) -> bool {
        self.waiting.is_some()
    }
}

/// The future a body runs for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RunningFuture {
    object: VirtualAddress,
    ty: TypeReference,
}

impl<P: LinuxTraceOps> Controller<P> {
    /// What a step of `kind` from a stopped thread's innermost frame
    /// follows across its awaits: the future whose body the frame runs.
    pub(super) fn await_step(
        &self,
        pid: Pid,
        kind: StepKind,
        start: &StepStart,
    ) -> Option<AwaitStep> {
        if !matches!(kind, StepKind::OverSource | StepKind::Out) {
            return None;
        }
        Some(AwaitStep {
            future: self.running_future(pid, start.code_instance)?,
            source: start.source.clone(),
            waiting: None,
            drop_glue: None,
        })
    }

    /// The future whose body a stopped thread's innermost activation runs
    /// in the code instance `selected`, or in the activation's own function
    /// when `None`, read from the pointer the body is passed. A body
    /// inlined into another's has a future of its own, or none the step
    /// can name, never the other's.
    fn running_future(&self, pid: Pid, selected: Option<CodeInstanceId>) -> Option<RunningFuture> {
        let inferior = self.inferior.as_ref()?;
        let module = self.modules.get(&inferior.loaded_module.id)?;
        let root = StackRoot::of_thread(pid);
        let stack = self.physical_stack(inferior, &root, 1).ok()?;
        let innermost = stack.frames.first()?;
        let address = inferior
            .loaded_module
            .image_address(innermost.context.instruction)
            .ok()?;
        let physical = self.module_image.locate(address).physical_instance?;
        let instance = selected.unwrap_or(physical);
        let coroutine = self
            .module_image
            .code_instance(instance)
            .and_then(|instance| self.module_image.function(instance.function))?
            .coroutine?;
        let code = Some((inferior.loaded_module.id, address));
        let modules = self.unwind_modules(inferior);
        let resolved = ResolvedFrame {
            id: crate::StackFrameId::new(0),
            presented: PresentedFrame::Physical,
            frame: None,
            code,
            scope: FrameScope::Function,
            registers: stack.registers(0),
            cfa: self.frame_cfa(pid, &modules, code, &innermost.registers),
            activation: 0,
            below_stack_pointer: self.below_stack_pointer(inferior, &root, innermost),
        };
        let key = module
            .variables
            .visible_object(
                address,
                (instance != physical).then_some(instance),
                "$future",
            )
            .ok()?;
        let mut runtime = self.frame_runtime(inferior, &root, &resolved, module);
        let mut budget =
            crate::inspection::InspectionBudget::new(crate::InspectionLimits::default());
        let located = module
            .variables
            .locate(key, Some(address), &mut runtime, &mut budget)
            .ok()?
            .ok()?;
        let crate::model::ValueStorage::Memory(object) = located.storage else {
            return None;
        };
        (located.ty == coroutine).then_some(RunningFuture {
            object,
            ty: TypeReference {
                image: module.loaded.image,
                id: coroutine,
            },
        })
    }

    /// The state a future that is not running is in, and its number.
    fn future_state(&self, pid: Pid, future: RunningFuture) -> Option<(u64, CoroutineStateKind)> {
        let inferior = self.inferior.as_ref()?;
        let module = self.module_of(future.ty)?;
        // The walk lists the future it began at last.
        let chain = self.with_module_stop(inferior, &module.loaded, pid, |stop| {
            futures::walk(module.image.as_ref(), stop, future.object, future.ty)
        });
        match chain.frames.last()?.kind {
            AsyncFrameKind::Coroutine { state, kind, .. } => Some((state, kind)),
            AsyncFrameKind::Leaf => None,
        }
    }

    /// Where a future that is not running resumes when it is next polled:
    /// the point its suspended state goes on from, or `None` when it is in
    /// no suspended state, having returned, or its resume point is unknown.
    fn resume_point(&self, pid: Pid, future: RunningFuture) -> Option<VirtualAddress> {
        let (state, CoroutineStateKind::Suspended { .. }) = self.future_state(pid, future)? else {
            return None;
        };
        let module = self.module_of(future.ty)?;
        let functions = module.image.coroutine_functions(future.ty.id);
        let address = super::async_frames::resume_address(&module.image, &functions, state)?;
        module.loaded.virtual_address(address).ok()
    }

    /// Follows the active step's future once the poll that ran its body
    /// has just returned: when it returned `Pending`, the step waits for the
    /// future to be polled again where it resumes, in place of the rest of
    /// its plan; when the future returned to its runtime as its task's own,
    /// the step ends there, with the task. `None` while the poll runs, or
    /// when the step goes on as any other does. The thread is left stopped
    /// either way.
    pub(super) fn follow_poll_return(
        &mut self,
        pid: Pid,
        kind: StepKind,
    ) -> Result<Option<Followed>> {
        let Some((execution, task, future, activation)) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { owner, start, .. } if self.runs_step(*owner, pid) => {
                    let awaiting = start.awaiting.as_ref().filter(|step| !step.waits())?;
                    Some((active.id, owner.task, awaiting.future, start.activation?))
                }
                _ => None,
            })
        else {
            return Ok(None);
        };
        let registers = self.ptrace.registers(pid)?;
        if !activation.has_returned(self.stack_position(pid, &registers)) {
            return Ok(None);
        }
        let Some(resumes) = self.resume_point(pid, future) else {
            let ended = task.filter(|_| {
                self.future_state(pid, future)
                    .is_some_and(|(_, state)| state == CoroutineStateKind::Returned)
                    && self.returned_to_runtime(pid, &registers)
            });
            return Ok(ended.map(|task| {
                record!("the step's task {task} finished");
                Followed::Ended(StopReason::TaskEnded {
                    kind,
                    task,
                    ending: TaskEnding::Finished,
                })
            }));
        };
        record!(
            "the poll of the future at {} returned pending; the step waits at {resumes}",
            future.object
        );
        let drop_glue = self.drop_glue(future.ty);
        let plan = std::iter::once(resumes).chain(drop_glue).collect();
        self.cleanup_plan_breakpoints(execution)?;
        self.install_additional_plan_breakpoints(execution, &plan)?;
        let start = self
            .active_step_mut()
            .expect("the step remained active while its future was pending");
        let mut awaiting = start.awaiting.take().expect("the step follows a future");
        awaiting.waiting = Some(resumes);
        awaiting.drop_glue = drop_glue;
        *start = StepStart {
            plan_addresses: plan,
            awaiting: Some(awaiting),
            ..StepStart::default()
        };
        Ok(Some(Followed::Waits))
    }

    /// Where the one function that drops every future of type `ty`
    /// begins: rustc names it `drop_glue<T>` for the type's full name.
    fn drop_glue(&self, ty: TypeReference) -> Option<VirtualAddress> {
        let module = self.module_of(ty)?;
        let info = module.image.type_info(ty)?;
        let mut name = String::from("drop_glue<");
        for segment in info.identity.as_deref()?.path.iter() {
            name.push_str(segment);
            name.push_str("::");
        }
        name.push_str(&info.name);
        name.push('>');
        let function = module
            .image
            .functions()
            .iter()
            .find(|function| *function.name == *name)?;
        let mut instances = module
            .image
            .instances_for_function(function.id)
            .filter(|instance| matches!(instance.kind, crate::CodeInstanceKind::OutOfLine));
        let entry = instances
            .next()?
            .ranges
            .iter()
            .map(|range| range.start)
            .min()?;
        if instances.next().is_some() {
            return None;
        }
        module.loaded.virtual_address(entry).ok()
    }

    /// Ends or goes on with a step whose future a thread at its drop glue
    /// is dropping. When the runtime's code drops it, the runtime cancelled
    /// its task, and the step ends; when the program's does, the step goes
    /// on in the frame that drops it, to that frame's next line.
    fn future_dropped(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
        kind: StepKind,
        future: RunningFuture,
    ) -> Result<()> {
        record!("thread {pid} drops the future the step waits for");
        self.follow_step(pid);
        let task =
            self.inferior
                .as_ref()
                .and_then(|inferior| match &inferior.active.as_ref()?.kind {
                    ActiveKind::Step { owner, .. } => owner.task,
                    _ => None,
                });
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let stack =
            self.physical_stack(inferior, &StackRoot::of_thread(pid), DEFAULT_MAX_FRAMES)?;
        // The future is dropped by the code that drops what holds it, as
        // the drop glue of each value around it passes it on.
        let dropper = (1..stack.frames.len())
            .find(|&level| {
                stack.lookup_address(level).is_none_or(|lookup| {
                    self.image_location(lookup)
                        .and_then(|location| location.function)
                        .is_none_or(|function| !is_drop_glue(&function.name))
                })
            })
            .filter(|&level| {
                stack
                    .lookup_address(level)
                    .is_some_and(|lookup| self.code_role(lookup) == Some(crate::CodeRole::Ordinary))
            });
        let Some(dropper) = dropper else {
            let reason = task.map_or_else(
                || StopReason::StepIncomplete {
                    kind,
                    description: "the future the step waited for was dropped".into(),
                },
                |task| StopReason::TaskEnded {
                    kind,
                    task,
                    ending: TaskEnding::Cancelled,
                },
            );
            return self.begin_visible_stop(pid, reason);
        };
        #[cfg(debug_assertions)]
        record!(
            "frame {dropper} drops it, in {}",
            stack
                .lookup_address(dropper)
                .and_then(|lookup| self.image_location(lookup))
                .and_then(|location| location.function)
                .map_or_else(|| "unnamed code".into(), |function| function.name)
        );
        let Some(start) = self.step_on_in_frame(pid, &stack, dropper, future)? else {
            return self.begin_visible_stop(
                pid,
                StopReason::StepIncomplete {
                    kind,
                    description: "the future the step waited for was dropped by code the step \
                                  cannot follow"
                        .into(),
                },
            );
        };
        let execution = self.active_execution()?;
        self.cleanup_plan_breakpoints(execution)?;
        self.install_additional_plan_breakpoints(
            execution,
            &start
                .plan_addresses
                .union(&start.panic_guards)
                .copied()
                .collect(),
        )?;
        if let Some(ActiveKind::Step {
            kind: running,
            start: active,
            ..
        }) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.active.as_mut())
            .map(|active| &mut active.kind)
        {
            // A step out ends where the step over the dropping frame's line
            // does, and is reported as the step the client asked for.
            *running = StepKind::OverSource;
            **active = start;
        }
        self.go_on_without_plan(pid, address, None)
    }

    /// A step over the line that the physical frame `level` of a stopped
    /// thread's stack is at, to the frame's next line or its return, which
    /// goes on after the frame dropped the future `dropped`.
    fn step_on_in_frame(
        &self,
        pid: Pid,
        stack: &super::frames::PhysicalStack,
        level: usize,
        dropped: RunningFuture,
    ) -> Result<Option<StepStart>> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let (Some(frame), Some(lookup)) = (stack.frames.get(level), stack.lookup_address(level))
        else {
            return Ok(None);
        };
        let Some(location) = self.image_location(lookup) else {
            return Ok(None);
        };
        let Some(instance) = location.physical_instance else {
            return Ok(None);
        };
        let Some(source) =
            super::frames::source_for_code_instance(&self.module_image, &location, instance)
        else {
            return Ok(None);
        };
        let modules = self.unwind_modules(inferior);
        let code = inferior
            .loaded_module
            .image_address(lookup)
            .ok()
            .map(|address| (inferior.loaded_module.id, address));
        let Ok(cfa) = self.frame_cfa(pid, &modules, code, &frame.registers) else {
            return Ok(None);
        };
        let mut plan = self.other_lines(instance, &source)?;
        if let Some(caller) = stack.frames.get(level + 1) {
            plan.insert(self.executable_return_address(pid, caller.context.instruction)?);
        }
        let registers = self.ptrace.registers(pid)?;
        Ok(Some(StepStart {
            source: Some(source),
            code_instance: Some(instance),
            physical_instance: Some(instance),
            activation: Some(self.stack_view(pid).activation(cfa)),
            stack_pointer: Some(self.stack_position(pid, &registers)),
            plan_addresses: plan,
            panic_guards: self.panic_entries(inferior),
            dropped: Some(dropped),
            ..StepStart::default()
        }))
    }

    /// The stop a step that goes on after its future was dropped reports
    /// as it completes: that the future was dropped.
    pub(super) fn dropped_step_reason(&self, reason: StopReason) -> StopReason {
        let dropped = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .is_some_and(
                |active| matches!(&active.kind, ActiveKind::Step { start, .. } if start.dropped.is_some()),
            );
        match reason {
            StopReason::Step { kind } if dropped => StopReason::FutureDropped { kind },
            reason => reason,
        }
    }

    /// Whether a thread whose future just returned is in its runtime's
    /// code, polling no future of the program's: it returned from its
    /// task's own future, not to an awaiter, nor to a future of the
    /// runtime's that the program awaits, such as a timeout.
    fn returned_to_runtime(&self, pid: Pid, registers: &nix::libc::user_regs_struct) -> bool {
        if !self
            .code_role(VirtualAddress::new(registers.rip))
            .is_some_and(super::stepping::is_runtime_role)
        {
            return false;
        }
        let Some(inferior) = self.inferior.as_ref() else {
            return false;
        };
        let Ok(stack) =
            self.physical_stack(inferior, &StackRoot::of_thread(pid), DEFAULT_MAX_FRAMES)
        else {
            return false;
        };
        stack.frames.iter().all(|frame| {
            self.image_location(frame.context.instruction)
                .and_then(|location| location.physical_instance)
                .and_then(|instance| self.module_image.code_instance(instance))
                .and_then(|instance| self.module_image.function(instance.function))
                .is_none_or(|function| function.coroutine.is_none())
        })
    }

    /// Goes on with a step that waits for its future at `address`, once a
    /// thread arrives there. The step's own future goes on as the step it
    /// was, from where it resumed; any other is not the step's, and passes.
    /// Returns whether the site was where the step waits and the arrival
    /// has been handled.
    pub(super) fn reach_resumed_future(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
    ) -> Result<bool> {
        let Some((kind, awaiting)) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { kind, start, .. } => start
                    .awaiting
                    .clone()
                    .filter(|awaiting| {
                        awaiting.waits()
                            && (awaiting.waiting == Some(address)
                                || awaiting.drop_glue == Some(address))
                    })
                    .map(|awaiting| (*kind, awaiting)),
                _ => None,
            })
        else {
            return Ok(false);
        };
        // Drop glue is passed the place it drops.
        if awaiting.drop_glue == Some(address) {
            if self.ptrace.registers(pid)?.rdi == awaiting.future.object.get() {
                self.future_dropped(pid, address, kind, awaiting.future)?;
            } else if self.barrier_active() {
                self.finish_barrier_if_ready()?;
            } else {
                self.repair_when_alone(pid, address)?;
            }
            return Ok(true);
        }
        if self.running_future(pid, None) != Some(awaiting.future) {
            if self.barrier_active() {
                self.finish_barrier_if_ready()?;
            } else {
                self.repair_when_alone(pid, address)?;
            }
            return Ok(true);
        }
        record!(
            "the future at {} resumed on thread {pid}",
            awaiting.future.object
        );
        self.follow_step(pid);
        let mut resumed =
            self.innermost_step_start(pid, kind, || self.presentation_for_thread(pid, None))?;
        // The step goes on past the line it began on, which the await is
        // on, or past the frame it began in.
        if kind == StepKind::OverSource {
            resumed.source.clone_from(&awaiting.source);
        }
        resumed.awaiting = Some(AwaitStep {
            waiting: None,
            ..awaiting
        });
        let execution = self.active_execution()?;
        self.cleanup_plan_breakpoints(execution)?;
        self.install_additional_plan_breakpoints(
            execution,
            &resumed
                .plan_addresses
                .union(&resumed.panic_guards)
                .copied()
                .collect(),
        )?;
        *self
            .active_step_mut()
            .expect("the step remained active as its future resumed") = resumed;
        self.go_on_without_plan(pid, address, Some(kind))?;
        Ok(true)
    }
}

/// Whether a function is one rustc makes to drop a value of some type.
fn is_drop_glue(name: &str) -> bool {
    name.starts_with("drop_glue<") || name.starts_with("drop_in_place<")
}

/// What a step does once the poll that ran its future's body returned.
pub(super) enum Followed {
    /// It waits for the future to be polled again.
    Waits,
    /// It ends, for this reason.
    Ended(StopReason),
}

/// Whether a step waits for its future to be polled again, and so cannot
/// end before then.
pub(super) fn waits(start: &StepStart) -> bool {
    start.awaiting.as_ref().is_some_and(AwaitStep::waits)
}
