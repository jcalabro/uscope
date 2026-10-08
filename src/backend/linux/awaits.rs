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
//! An optimized build may inline one async function's body into another's,
//! where it has no resume points of its own and may not name its future.
//! The step then follows the future of the function it is inlined into,
//! whose poll runs both, and once that poll returns `Pending`, waits for
//! the future to come back to the inlined body's statements, or to those
//! on another line of the code it is inlined into. A future names its
//! task, so where even that future's address is unavailable, the task
//! does.
//!
//! A step may also begin in a task no thread runs, at one of the async
//! functions it awaits in: it begins waiting for that function's future,
//! as if a poll of it had just returned `Pending`.
//!
//! A future may be dropped while the step waits for it: its runtime drops
//! a task's future as it cancels the task, and a `select!` or a timeout
//! drops a future it no longer awaits. The step watches the future's drop
//! glue for that. Dropped by its runtime, the future's task was cancelled,
//! and the step ends there; dropped by the program's code, the step goes on
//! in that code to its next line.

use std::collections::BTreeSet;

use nix::unistd::Pid;

use crate::protocol::{
    ExceptionDisposition, ExecutionId, ProcessId, ResumeScope, StepKind, StopId, StopReason,
    TaskEnding,
};
use crate::runtime_model::futures::{self, AsyncFrameKind};
use crate::unwind::DEFAULT_MAX_FRAMES;
use crate::{
    CodeInstanceId, CodeInstanceKind, CoroutineStateKind, Error, ExecutionContext, FunctionId,
    InlineFrameLookup, Result, SourceLocation, StackFrameId, TypeReference, VirtualAddress,
};

use super::frames::{FrameScope, ResolvedFrame, StackRoot};
use super::native::LinuxTraceOps;
use super::{ActiveKind, Controller, StepOwner, StepStart};
use crate::PresentedFrame;

/// The future whose body a step runs, which the step follows across its
/// polls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AwaitStep {
    /// The future whose poll runs the step's body: the body's own, or the
    /// future of the function the body is inlined into.
    future: RunningFuture,
    /// For a body inlined into another's, where the step goes on.
    inlined: Option<Inlined>,
    /// The line the step began on, which it goes on past once the future
    /// resumes.
    source: Option<SourceLocation>,
    /// Where the future resumes, once a poll of it returned `Pending` and
    /// the step waits for the next.
    waiting: BTreeSet<VirtualAddress>,
    /// Where the code that drops the future begins, which the step watches
    /// while it waits, when one function drops every future of its type.
    drop_glue: Option<VirtualAddress>,
}

/// An async function's body inlined into another's, which a step runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Inlined {
    /// The future whose poll runs the body.
    poll: RunningFuture,
    /// The body's function, which the future must await once only when the
    /// body names no future of its own.
    function: FunctionId,
    /// The body's code.
    instances: BTreeSet<CodeInstanceId>,
    /// Where the step goes on once its future is polled again: the body's
    /// statements, and those on another line of the code the body is
    /// inlined into, but none that only resuming runs.
    statements: BTreeSet<VirtualAddress>,
}

impl AwaitStep {
    /// Whether the step waits for its future to be polled again.
    pub(super) fn waits(&self) -> bool {
        !self.waiting.is_empty()
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
        let inline = start
            .code_instance
            .and_then(|instance| self.module_image.code_instance(instance))
            .is_some_and(|instance| matches!(instance.kind, CodeInstanceKind::Inline { .. }));
        let (future, inlined) = if inline {
            self.inlined_body(pid, start)?
        } else {
            (self.running_future(pid, start.code_instance)?, None)
        };
        Some(AwaitStep {
            future,
            inlined,
            source: start.source.clone(),
            waiting: BTreeSet::new(),
            drop_glue: None,
        })
    }

    /// For a step in an async function's body inlined into another's, the
    /// body's future, or where it names none, that of the function it is
    /// inlined into, and where the step goes on once the future is polled
    /// again.
    fn inlined_body(
        &self,
        pid: Pid,
        start: &StepStart,
    ) -> Option<(RunningFuture, Option<Inlined>)> {
        let instance = start.code_instance?;
        let info = self.module_image.code_instance(instance)?;
        let function = info.function;
        if !matches!(info.kind, CodeInstanceKind::Inline { .. })
            || self.module_image.function(function)?.coroutine.is_none()
        {
            return None;
        }
        let poll = self.running_future(pid, None)?;
        let future = self.running_future(pid, Some(instance)).unwrap_or(poll);
        let registers = self.ptrace.registers(pid).ok()?;
        let location = self.image_location(VirtualAddress::new(registers.rip))?;
        let InlineFrameLookup::Unique(chain) = &location.inline_frames else {
            return None;
        };
        let depth = chain.instances.iter().position(|id| *id == instance)?;
        // The body's own statements, and those on another line of each
        // function it is inlined into, out to the one that runs.
        let mut statements = self.body_statements(instance).ok()?;
        for outer in chain.instances[..depth]
            .iter()
            .copied()
            .chain(location.physical_instance)
        {
            let line =
                super::frames::source_for_code_instance(&self.module_image, &location, outer)?;
            statements.extend(self.other_lines(outer, &line).ok()?);
        }
        Some((
            future,
            Some(Inlined {
                poll,
                function,
                instances: BTreeSet::from([instance]),
                statements: self.without_resume_code(statements),
            }),
        ))
    }

    /// Whether a step in an inlined async body that has left the body for
    /// the code at `address`, which is not where the step goes on, must
    /// go on: the code the body is inlined into awaits it there, which the
    /// step follows while the body may be pending. A body that names its
    /// future says whether it is; one that does not may be.
    pub(super) fn left_inlined_body_pending(
        &self,
        pid: Pid,
        start: &StepStart,
        address: VirtualAddress,
    ) -> bool {
        let Some((awaiting, inlined)) = start
            .awaiting
            .as_ref()
            .and_then(|awaiting| Some((awaiting, awaiting.inlined.as_ref()?)))
        else {
            return false;
        };
        !inlined.statements.contains(&address)
            && (awaiting.future == inlined.poll
                || matches!(
                    self.future_state(pid, awaiting.future),
                    Some((_, CoroutineStateKind::Suspended { .. }))
                ))
    }

    /// The statements of an async function's body inlined into another's,
    /// less those on its header's line: entering the body there dispatches
    /// on its state, which resuming it does too.
    fn body_statements(&self, instance: CodeInstanceId) -> Result<BTreeSet<VirtualAddress>> {
        let header = self
            .module_image
            .code_instance(instance)
            .and_then(|instance| self.module_image.function(instance.function))
            .and_then(|function| function.declaration.clone());
        self.statements_of(instance, header.as_ref())
    }

    /// Statements less those that only resuming a coroutine runs, such as
    /// its dispatch on its state.
    fn without_resume_code(
        &self,
        statements: BTreeSet<VirtualAddress>,
    ) -> BTreeSet<VirtualAddress> {
        let Some(inferior) = self.inferior.as_ref() else {
            return statements;
        };
        statements
            .into_iter()
            .filter(|address| {
                inferior
                    .loaded_module
                    .image_address(*address)
                    .is_ok_and(|address| !self.module_image.is_resume_code(address))
            })
            .collect()
    }

    /// Whether the awaits of `future` hold more than one future of
    /// `function`, which a step in its inlined body cannot tell apart.
    fn awaits_twice(&self, pid: Pid, future: RunningFuture, function: FunctionId) -> bool {
        let Some(inferior) = self.inferior.as_ref() else {
            return false;
        };
        let Some(module) = self.module_of(future.ty) else {
            return false;
        };
        let chain = self.with_module_stop(inferior, &module.loaded, pid, |stop| {
            futures::walk(module.image.as_ref(), stop, future.object, future.ty)
        });
        chain
            .frames
            .iter()
            .filter(|frame| matches!(frame.kind, AsyncFrameKind::Coroutine { .. }))
            .filter(|frame| {
                module
                    .image
                    .coroutine_functions(frame.ty.id)
                    .iter()
                    .any(|candidate| candidate.id == function)
            })
            .count()
            > 1
    }

    /// Begins a step of `kind` in a task no thread runs, from the async
    /// function `frame` selects among its awaits, or the innermost one that
    /// awaits it: the step waits for the function's future to resume, and
    /// goes on from there as the step it is. A step in goes on as a step
    /// over, since the call the line makes is the await already under way.
    pub(super) fn step_suspended(
        &mut self,
        (process_id, stop_id): (ProcessId, StopId),
        task: crate::TaskId,
        frame: StackFrameId,
        kind: StepKind,
        scope: ResumeScope,
        exception: ExceptionDisposition,
    ) -> Result<ExecutionId> {
        let parked = || Error::TaskParked(task);
        if !matches!(
            kind,
            StepKind::IntoSource | StepKind::OverSource | StepKind::Out
        ) || matches!(scope, ResumeScope::Thread(_))
        {
            return Err(parked());
        }
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let root = self.stack_root(stop_id, ExecutionContext::Task(task))?;
        let super::frames::RootOrigin::Suspended { reader, .. } = root.origin else {
            return Err(parked());
        };
        let stack = self.async_stack(inferior, &root)?.ok_or_else(parked)?;
        let level = usize::try_from(frame.get()).expect("u32 fits usize");
        let selected = (level..stack.futures.len())
            .find(|&index| matches!(stack.futures[index].kind, AsyncFrameKind::Coroutine { .. }))
            .ok_or_else(|| {
                Error::FrameStepUnsupported("the task's frames hold no async function".into())
            })?;
        if stack.futures[selected].ty.image != inferior.loaded_module.image {
            return Err(Error::FrameStepUnsupported(
                "the async function is in a library, whose awaits steps do not follow".into(),
            ));
        }
        let own = self.named_by_body(&stack.futures[selected]);
        let (future, inlined, waiting) = match self.resume_point(reader, own) {
            Some(resumes) => (own, None, BTreeSet::from([resumes])),
            None => self.suspended_inline_body(&stack, selected)?,
        };
        let drop_glue = self.drop_glue(future.ty);
        record!(
            "a step of task {task} waits for the future at {} at {waiting:?}",
            future.object
        );
        let requested = kind;
        let kind = match kind {
            StepKind::IntoSource => StepKind::OverSource,
            kind => kind,
        };
        let start = StepStart {
            plan_addresses: waiting.iter().copied().chain(drop_glue).collect(),
            awaiting: Some(AwaitStep {
                future,
                inlined,
                source: stack.frames[selected].source.clone(),
                waiting,
                drop_glue,
            }),
            ..StepStart::default()
        };
        self.begin_execution(
            process_id,
            stop_id,
            scope,
            ActiveKind::Step {
                owner: StepOwner {
                    thread: reader,
                    task: Some(task),
                },
                kind,
                requested,
                start: Box::new(start),
                progress_owed: false,
            },
            exception,
        )
    }

    /// A suspended future as its body names it: the type its awaiter holds
    /// it as is a description of its own, while the body's polls find it
    /// as the type the body names.
    fn named_by_body(&self, future: &futures::AsyncFrame) -> RunningFuture {
        let ty = self
            .module_image
            .coroutine_functions(future.ty.id)
            .first()
            .and_then(|function| function.coroutine)
            .map_or(future.ty, |id| TypeReference {
                image: future.ty.image,
                id,
            });
        RunningFuture {
            object: future.object,
            ty,
        }
    }

    /// For a step of a suspended task from frame `selected` of its awaits,
    /// whose async function's body is inlined into another's, the future
    /// whose poll runs it, that of the function it is inlined into, and
    /// where the step goes on once that future is polled again: the body's
    /// statements, and those on another line of each function it is
    /// inlined into.
    fn suspended_inline_body(
        &self,
        stack: &super::async_frames::AsyncStack,
        selected: usize,
    ) -> Result<(RunningFuture, Option<Inlined>, BTreeSet<VirtualAddress>)> {
        let unknown = || {
            Error::FrameStepUnsupported(
                "the async function waits at no await whose resumption is known".into(),
            )
        };
        let function_of = |index: usize| {
            self.module_image
                .coroutine_functions(stack.futures[index].ty.id)
                .first()
                .map(|function| function.id)
        };
        let out_of_line = |function: FunctionId| {
            self.module_image
                .instances_for_function(function)
                .find(|instance| matches!(instance.kind, CodeInstanceKind::OutOfLine))
        };
        // The innermost future out from the selected one whose function
        // runs out of line, which the inlined bodies run in.
        let (running, physical) = (selected + 1..stack.futures.len())
            .filter(|&index| matches!(stack.futures[index].kind, AsyncFrameKind::Coroutine { .. }))
            .find_map(|index| Some((index, out_of_line(function_of(index)?)?)))
            .ok_or_else(unknown)?;
        let inlined_in = |function: FunctionId| {
            self.module_image
                .instances_for_function(function)
                .filter(|instance| {
                    matches!(instance.kind, CodeInstanceKind::Inline { .. })
                        && instance
                            .ranges
                            .first()
                            .is_some_and(|range| physical.contains(range.start))
                })
                .map(|instance| instance.id)
                .collect::<BTreeSet<_>>()
        };
        let function = function_of(selected).ok_or_else(unknown)?;
        let instances = inlined_in(function);
        let mut statements = BTreeSet::new();
        for instance in &instances {
            statements.extend(self.body_statements(*instance)?);
        }
        for index in selected + 1..=running {
            let (Some(outer), Some(line)) = (function_of(index), &stack.frames[index].source)
            else {
                continue;
            };
            let outers = if index == running {
                BTreeSet::from([physical.id])
            } else {
                inlined_in(outer)
            };
            for instance in outers {
                statements.extend(self.other_lines(instance, line)?);
            }
        }
        let statements = self.without_resume_code(statements);
        if instances.is_empty() || statements.is_empty() {
            return Err(unknown());
        }
        Ok((
            self.named_by_body(&stack.futures[selected]),
            Some(Inlined {
                poll: self.named_by_body(&stack.futures[running]),
                function,
                instances,
                statements: statements.clone(),
            }),
            statements,
        ))
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
        let Some((execution, task, awaiting, activation)) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { owner, start, .. } if self.runs_step(*owner, pid) => {
                    let awaiting = start.awaiting.as_ref().filter(|step| !step.waits())?;
                    Some((active.id, owner.task, awaiting.clone(), start.activation?))
                }
                _ => None,
            })
        else {
            return Ok(None);
        };
        let future = awaiting.future;
        let registers = self.ptrace.registers(pid)?;
        if !activation.has_returned(self.stack_position(pid, &registers)) {
            return Ok(None);
        }
        let waiting = match &awaiting.inlined {
            None => self.resume_point(pid, future).into_iter().collect(),
            Some(inlined) => match self.future_state(pid, future) {
                Some((_, CoroutineStateKind::Suspended { .. })) => {
                    if future == inlined.poll && self.awaits_twice(pid, future, inlined.function) {
                        return Ok(Some(Followed::Ended(StopReason::StepIncomplete {
                            kind,
                            description: TWICE.into(),
                        })));
                    }
                    inlined.statements.clone()
                }
                _ => BTreeSet::new(),
            },
        };
        if waiting.is_empty() {
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
        }
        record!(
            "the poll of the future at {} returned pending; the step waits at {waiting:?}",
            future.object
        );
        let drop_glue = self.drop_glue(future.ty);
        let plan = waiting.iter().copied().chain(drop_glue).collect();
        self.cleanup_plan_breakpoints(execution)?;
        self.install_additional_plan_breakpoints(execution, &plan)?;
        let start = self
            .active_step_mut()
            .expect("the step remained active while its future was pending");
        let mut awaiting = start.awaiting.take().expect("the step follows a future");
        awaiting.waiting = waiting;
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
        let Some((kind, owner, awaiting)) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step {
                    kind, owner, start, ..
                } => start
                    .awaiting
                    .clone()
                    .filter(|awaiting| {
                        awaiting.waits()
                            && (awaiting.waiting.contains(&address)
                                || awaiting.drop_glue == Some(address))
                    })
                    .map(|awaiting| (*kind, *owner, awaiting)),
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
        // An inlined body's code may hold its future nowhere; the future
        // names its task, which then names the future.
        let polled = awaiting
            .inlined
            .as_ref()
            .map_or(awaiting.future, |inlined| inlined.poll);
        let ours = match self.running_future(pid, None) {
            Some(running) => running == polled,
            None => {
                awaiting.inlined.is_some() && owner.task.is_some() && self.runs_step(owner, pid)
            }
        };
        if !ours {
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
        let presentation = match &awaiting.inlined {
            None => self.presentation_for_thread(pid, None)?,
            Some(inlined) => match self.reentered(address, inlined, awaiting.source.as_ref()) {
                Reentry::Body(presentation) if kind == StepKind::Out || !presentation.1 => {
                    presentation.0
                }
                // A statement on another line than the step's of the body
                // or of the code it is inlined into is where the step
                // ends.
                Reentry::Body(_) | Reentry::Outside => {
                    self.begin_visible_stop(pid, StopReason::Step { kind })?;
                    return Ok(true);
                }
            },
        };
        let mut resumed = self.innermost_step_start(pid, kind, || Ok(presentation))?;
        // The step goes on past the line it began on, which the await is
        // on, or past the frame it began in.
        if kind == StepKind::OverSource {
            resumed.source.clone_from(&awaiting.source);
        }
        resumed.awaiting = Some(AwaitStep {
            waiting: BTreeSet::new(),
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

/// Where a future whose step is in an inlined body comes back to.
enum Reentry {
    /// The body, presented as the step's frame, and whether on another line
    /// than the step's.
    Body((crate::FramePresentation, bool)),
    /// The code the body is inlined into, having returned meanwhile.
    Outside,
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Where the future of a step in the inlined body `inlined` has come
    /// back to at `address`, which began on line `source`.
    fn reentered(
        &self,
        address: VirtualAddress,
        inlined: &Inlined,
        source: Option<&SourceLocation>,
    ) -> Reentry {
        let Some(location) = self.image_location(address) else {
            return Reentry::Outside;
        };
        let InlineFrameLookup::Unique(chain) = &location.inline_frames else {
            return Reentry::Outside;
        };
        let Some(depth) = chain
            .instances
            .iter()
            .position(|instance| inlined.instances.contains(instance))
        else {
            return Reentry::Outside;
        };
        let instance = chain.instances[depth];
        let moved =
            super::frames::source_for_code_instance(&self.module_image, &location, instance)
                .is_some_and(|line| super::frames::source_line_changed(source, Some(&line)));
        Reentry::Body((
            crate::FramePresentation {
                instruction: address,
                frame: PresentedFrame::Inline(instance),
                hidden_inline_frames: u32::try_from(chain.instances.len() - depth - 1)
                    .unwrap_or(u32::MAX),
            },
            moved,
        ))
    }
}

/// Why a step in an inlined body cannot follow its future.
const TWICE: &str = "the async function the step is in is awaited twice in its task, and \
                     inlined, so the step cannot tell which is its own";

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
