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

use nix::unistd::Pid;

use crate::protocol::StepKind;
use crate::runtime_model::futures::{self, AsyncFrameKind};
use crate::{
    CodeInstanceId, CoroutineStateKind, Error, Result, SourceLocation, TypeReference,
    VirtualAddress,
};

use super::breakpoints::install_plan_breakpoint;
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

    /// Where a future that is not running resumes when it is next polled:
    /// the point its suspended state goes on from, or `None` when it is in
    /// no suspended state, having returned, or its resume point is unknown.
    fn resume_point(&self, pid: Pid, future: RunningFuture) -> Option<VirtualAddress> {
        let inferior = self.inferior.as_ref()?;
        let module = self.module_of(future.ty)?;
        // The walk lists the future it began at last.
        let chain = self.with_module_stop(inferior, &module.loaded, pid, |stop| {
            futures::walk(module.image.as_ref(), stop, future.object, future.ty)
        });
        let AsyncFrameKind::Coroutine {
            state,
            kind: CoroutineStateKind::Suspended { .. },
            ..
        } = chain.frames.last()?.kind
        else {
            return None;
        };
        let functions = module.image.coroutine_functions(future.ty.id);
        let address = super::async_frames::resume_address(&module.image, &functions, state)?;
        module.loaded.virtual_address(address).ok()
    }

    /// When the poll that ran the active step's body has just returned
    /// `Pending`, waits for the step's future to be polled again where it
    /// resumes, in place of the rest of the step's plan. Returns whether
    /// the step now waits; the thread is then left stopped, for the caller
    /// to let it run on.
    pub(super) fn await_pending_poll(&mut self, pid: Pid) -> Result<bool> {
        let Some((execution, future, activation)) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { owner, start, .. } if self.runs_step(*owner, pid) => {
                    let awaiting = start.awaiting.as_ref().filter(|step| !step.waits())?;
                    Some((active.id, awaiting.future, start.activation?))
                }
                _ => None,
            })
        else {
            return Ok(false);
        };
        let registers = self.ptrace.registers(pid)?;
        if !activation.has_returned(self.stack_position(pid, &registers)) {
            return Ok(false);
        }
        let Some(resumes) = self.resume_point(pid, future) else {
            return Ok(false);
        };
        record!(
            "the poll of the future at {} returned pending; the step waits at {resumes}",
            future.object
        );
        self.cleanup_plan_breakpoints(execution)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        install_plan_breakpoint(&self.ptrace, inferior, resumes, execution)?;
        let start = self
            .active_step_mut()
            .expect("the step remained active while its future was pending");
        let mut awaiting = start.awaiting.take().expect("the step follows a future");
        awaiting.waiting = Some(resumes);
        *start = StepStart {
            plan_addresses: std::collections::BTreeSet::from([resumes]),
            awaiting: Some(awaiting),
            ..StepStart::default()
        };
        Ok(true)
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
                    .filter(|awaiting| awaiting.waiting == Some(address))
                    .map(|awaiting| (*kind, awaiting)),
                _ => None,
            })
        else {
            return Ok(false);
        };
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

/// Whether a step waits for its future to be polled again, and so cannot
/// end before then.
pub(super) fn waits(start: &StepStart) -> bool {
    start.awaiting.as_ref().is_some_and(AwaitStep::waits)
}
