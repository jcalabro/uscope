//! A suspended task's frames: the chain of awaits its future holds, from
//! the future it waits on out to the async function it began in. No
//! thread runs them, so they have no registers; each async function's
//! variables are the members its state keeps.

use std::sync::Arc;

use crate::debug_info::Located;
use crate::inspection::InspectionBudget;
use crate::model::{FrameMetadata, ValueStorage};
use crate::protocol::{PresentedFrame, StopId};
use crate::runtime_model::futures::{self, AsyncFrameKind, ChainEnd};
use crate::{
    Backtrace, CallFrameUnavailableReason, CodeInstanceKind, CodeRole, CoroutineInfo, Error,
    FrameKind, ImageAddress, LoadedModule, ModuleImage, RecordMemberLayout, Result, StackFrame,
    StackFrameId, StackSegment, TypeReference, UnwindTermination, Variable, VariableKind,
    VariableQuery, VariableUnavailableReason, VirtualAddress,
};

use super::frames::{FrameRegisters, FrameScope, ResolvedFrame, RootOrigin, StackRoot};
use super::inspection::variable_context;
use super::native::InspectionOps;
use super::{Controller, Inferior, RuntimeModule};

/// A suspended task's frames, innermost first, each with the future it
/// was read from, and why they end.
pub(super) struct AsyncStack {
    pub(super) frames: Vec<StackFrame>,
    pub(super) futures: Vec<futures::AsyncFrame>,
    pub(super) termination: UnwindTermination,
}

/// One variable of a suspended async frame: a member of its future.
pub(super) struct SavedVariable<'a> {
    pub(super) name: &'a Arc<str>,
    pub(super) ty: TypeReference,
    pub(super) address: VirtualAddress,
    pub(super) kind: VariableKind,
    pub(super) member: &'a crate::RecordMember,
}

impl<P: InspectionOps> Controller<P> {
    /// The frames of a suspended task's stack, or `None` for a stack a
    /// thread's or task's registers begin.
    pub(super) fn async_stack(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
    ) -> Result<Option<AsyncStack>> {
        let RootOrigin::Suspended {
            future,
            ty,
            module,
            reader,
        } = &root.origin
        else {
            return Ok(None);
        };
        let runtime_module = self.suspended_module(module, *ty)?;
        let image = runtime_module.image.as_ref();
        let chain = self.with_module_stop(inferior, module, *reader, |stop| {
            futures::walk(image, stop, *future, *ty)
        });
        let frames = chain
            .frames
            .iter()
            .enumerate()
            .map(|(level, frame)| {
                let level = u32::try_from(level).expect("an await chain's depth fits u32");
                async_frame(image, module, level, frame)
            })
            .collect();
        let termination = match chain.end {
            ChainEnd::Leaf | ChainEnd::Unresumed | ChainEnd::Finished => {
                UnwindTermination::Complete
            }
            ChainEnd::NoAwaitee => UnwindTermination::BrokenAwaitChain {
                reason: "the debug information does not say what the innermost async \
                         function awaits"
                    .into(),
            },
            ChainEnd::Broken(reason) => UnwindTermination::BrokenAwaitChain { reason },
            ChainEnd::TooDeep => UnwindTermination::DepthLimit,
            ChainEnd::Cycle => UnwindTermination::CycleDetected,
        };
        Ok(Some(AsyncStack {
            frames,
            futures: chain.frames,
            termination,
        }))
    }

    /// A suspended task's backtrace, or `None` for any other stack.
    pub(super) fn async_backtrace(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
    ) -> Result<Option<Backtrace>> {
        Ok(self.async_stack(inferior, root)?.map(|stack| Backtrace {
            context: root.context,
            frames: stack.frames.into(),
            termination: stack.termination,
        }))
    }

    /// One frame of a suspended task, and what evaluates its variables, or
    /// `None` for any other stack.
    pub(super) fn resolve_async_frame(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        id: StackFrameId,
    ) -> Result<Option<ResolvedFrame>> {
        let Some(stack) = self.async_stack(inferior, root)? else {
            return Ok(None);
        };
        let level = usize::try_from(id.get()).expect("u32 fits usize");
        let Some(frame) = stack.frames.get(level).cloned() else {
            return Err(Error::FrameNotFound {
                frame: id,
                frames: u32::try_from(stack.frames.len()).expect("frame count fits u32"),
            });
        };
        let scope = match &stack.futures[level] {
            futures::AsyncFrame {
                object,
                ty,
                kind: AsyncFrameKind::Coroutine { state, .. },
            } => FrameScope::Suspended {
                object: *object,
                ty: *ty,
                state: *state,
            },
            _ => FrameScope::Unavailable,
        };
        let code = frame
            .module
            .zip(frame.instruction)
            .and_then(|(id, address)| {
                let module = self.modules.get(&id)?;
                Some((id, module.loaded.image_address(address).ok()?))
            });
        Ok(Some(ResolvedFrame {
            id,
            presented: PresentedFrame::Physical,
            frame: Some(frame),
            code,
            scope,
            registers: FrameRegisters::Discarded,
            cfa: Err(VariableUnavailableReason::CallFrameUnavailable(
                CallFrameUnavailableReason::Suspended,
            )
            .into()),
            activation: 0,
            below_stack_pointer: None,
        }))
    }

    /// The variables a suspended async frame shows, each read from its
    /// future: those of `query`.
    pub(super) fn async_variables(
        &self,
        stop_id: StopId,
        root: &StackRoot,
        resolved: &ResolvedFrame,
        query: &VariableQuery,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<Variable>> {
        let FrameScope::Suspended { ty, .. } = resolved.scope else {
            return Err(Error::VariableContextUnsupported);
        };
        let module = self.module_of(ty).ok_or(Error::AddressOutsideModule)?;
        let saved = self.saved_variables(resolved)?;
        let wanted = |variable: &SavedVariable<'_>| match query {
            VariableQuery::All => true,
            VariableQuery::Name(name) => **variable.name == **name,
            VariableQuery::Global(_) => false,
        };
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let mut runtime = self.frame_runtime(inferior, root, resolved, module);
        let context = variable_context(stop_id, root.context, resolved.id, module, None);
        let mut variables = Vec::new();
        for variable in saved.iter().filter(|variable| wanted(variable)) {
            if budget.consume_variable_value().is_err() {
                break;
            }
            let located = Located {
                ty: variable.ty.id,
                storage: ValueStorage::Memory(variable.address),
            };
            let inspected =
                module
                    .variables
                    .materialize(&located, context, &mut runtime, budget)?;
            variables.push(Variable {
                kind: variable.kind,
                global: None,
                name: Arc::clone(variable.name),
                declaration: variable.member.declaration.clone(),
                type_info: inspected.type_info,
                unresolved_shape: None,
                state: inspected.state,
            });
        }
        if let VariableQuery::Name(name) = query
            && variables.is_empty()
        {
            return Err(Error::VariableNotFound(name.clone()));
        }
        Ok(variables)
    }

    /// The variables a suspended async frame holds, in its future.
    pub(super) fn saved_variables<'a>(
        &'a self,
        resolved: &ResolvedFrame,
    ) -> Result<Vec<SavedVariable<'a>>> {
        let FrameScope::Suspended { object, ty, state } = resolved.scope else {
            return Ok(Vec::new());
        };
        let module = self.module_of(ty).ok_or(Error::AddressOutsideModule)?;
        let Some(Ok(coroutine)) = module.image.coroutine(ty.id) else {
            return Ok(Vec::new());
        };
        let Some(state) = coroutine.state(state) else {
            return Ok(Vec::new());
        };
        Ok(saved(coroutine, state, object, ty))
    }

    /// The module whose image describes a suspended task's future.
    fn suspended_module(&self, module: &LoadedModule, ty: TypeReference) -> Result<&RuntimeModule> {
        self.modules
            .get(&module.id)
            .filter(|found| found.loaded.image == ty.image)
            .ok_or(Error::StaleModuleImage)
    }
}

/// The variables a coroutine of type `ty` at `object` holds in `state`.
fn saved<'a>(
    coroutine: &'a CoroutineInfo,
    state: &'a crate::CoroutineState,
    object: VirtualAddress,
    ty: TypeReference,
) -> Vec<SavedVariable<'a>> {
    coroutine
        .variables(state)
        .filter_map(|(member, capture)| {
            let RecordMemberLayout::ByteOffset(offset) = member.layout else {
                return None;
            };
            Some(SavedVariable {
                name: member.name.as_ref()?,
                ty: TypeReference {
                    image: ty.image,
                    id: member.type_ref.id,
                },
                address: VirtualAddress::new(object.get().wrapping_add(offset)),
                // An async function's captures are its arguments.
                kind: if capture && coroutine.kind == crate::CoroutineKind::AsyncFunction {
                    VariableKind::Parameter
                } else {
                    VariableKind::Local
                },
                member,
            })
        })
        .collect()
}

/// A frame of a suspended task's chain of awaits.
fn async_frame(
    image: &ModuleImage,
    module: &LoadedModule,
    level: u32,
    frame: &futures::AsyncFrame,
) -> StackFrame {
    let mut built = match &frame.kind {
        AsyncFrameKind::Coroutine {
            state, location, ..
        } => {
            let functions = image.coroutine_functions(frame.ty.id);
            let function = functions.first().map(|function| (*function).clone());
            let role = function
                .as_ref()
                .map_or(CodeRole::Ordinary, |function| function.role);
            let resumes = resume_address(image, &functions, *state)
                .map(|address| VirtualAddress::new(address.get().wrapping_add(module.load_bias)));
            StackFrame::from_parts(
                level,
                FrameKind::Async {
                    object: frame.object,
                },
                Some(module.id),
                resumes,
                FrameMetadata {
                    code_instance: None,
                    function,
                    source: location.clone(),
                    symbol: None,
                    role,
                },
            )
        }
        AsyncFrameKind::Leaf => StackFrame::from_parts(
            level,
            FrameKind::Awaited {
                object: frame.object,
                ty: frame.ty,
            },
            Some(module.id),
            None,
            FrameMetadata {
                code_instance: None,
                function: None,
                source: None,
                symbol: None,
                role: CodeRole::Ordinary,
            },
        ),
    };
    built.segment = StackSegment::Task;
    built
}

/// Where the one out-of-line function that runs a coroutine resumes in
/// `state`, when the debugger decoded its dispatch.
fn resume_address(
    image: &ModuleImage,
    functions: &[&crate::FunctionInfo],
    state: u64,
) -> Option<ImageAddress> {
    let mut instances = functions
        .iter()
        .flat_map(|function| image.instances_for_function(function.id))
        .filter(|instance| matches!(instance.kind, CodeInstanceKind::OutOfLine));
    let instance = instances.next()?;
    if instances.next().is_some() {
        return None;
    }
    let points = image.resume_points(instance.id)?.ok()?;
    Some(points.point(state)?.address)
}
