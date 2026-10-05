//! Variable, expression, and global inspection at a stopped snapshot.

use std::collections::BTreeMap;
use std::sync::Arc;

use nix::unistd::Pid;

use crate::debug_info::{VariableContext, VariableRegister, VariableRuntime, VariableRuntimeError};
use crate::inspection::{InspectionBudget, MAX_INSPECTION_LIMITS};
use crate::protocol::{GlobalVariableQuery, StopId, VariableQuery};
use crate::{
    CallFrameUnavailableReason, CodeInstanceId, Error, GlobalVariablePage, GlobalVariableReference,
    ImageAddress, InspectedValue, LoadedGlobalVariableInfo, LoadedModule, MemoryReadCompletion,
    RegisterSnapshot, Result, StackFrameId, TlsUnavailableReason, UnwindTermination,
    ValueExpression, ValueIndexRange, ValuePathStep, VariableSnapshot, VariableUnavailableReason,
    VirtualAddress,
};

use super::frames::{FrameRegisters, FrameScope, ResolvedFrame};
use super::memory::read_logical_memory;
use super::native::InspectionOps;
use super::registers::{
    Fxsave, x86_64_caller_variable_register, x86_64_general_variable_register,
    x86_64_register_snapshot, x86_64_xmm_variable_register,
};
use super::{
    BreakpointSite, Controller, ExpressionRoot, ExpressionRootKind, Inferior,
    MAX_VALUE_CHILD_PAGE_LIMIT, MAX_VALUE_EXPRESSION_DEREFERENCES, MAX_VALUE_EXPRESSION_STEPS,
    RuntimeModule, debug_pid, debug_thread_id, validate_image_current, validate_public_stop,
    validate_stopped_thread,
};

impl<P: InspectionOps> Controller<P> {
    pub(super) fn registers(
        &self,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
    ) -> Result<RegisterSnapshot> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_stopped_thread(inferior, pid)?;
        let native = self.ptrace.registers(pid)?;
        let resolved = if frame == StackFrameId::INNERMOST {
            None
        } else {
            Some(self.resolve_frame(inferior, pid, frame)?)
        };
        let caller = match resolved.as_ref().map(|frame| &frame.registers) {
            Some(FrameRegisters::Caller(registers)) => Some(registers),
            Some(FrameRegisters::Thread(_)) | None => None,
        };
        Ok(x86_64_register_snapshot(
            self.revision,
            pid,
            self.module_image.target(),
            &native,
            caller,
        ))
    }

    /// Returns the module describing a frame's function, the frame's address
    /// in its image, and the frame's source scope, or `None` when no single
    /// function scope applies: code without debug information, or an
    /// innermost frame with no single inline chain.
    pub(super) fn frame_scope(
        &self,
        frame: &ResolvedFrame,
    ) -> Option<(&RuntimeModule, ImageAddress, Option<CodeInstanceId>)> {
        let (module, address) = frame.code?;
        let selected = match frame.scope {
            FrameScope::Unavailable => return None,
            FrameScope::Function => None,
            FrameScope::Inline(instance) => Some(instance),
        };
        Some((self.modules.get(&module)?, address, selected))
    }

    /// Reads a frame's registers and memory for values `module` describes.
    pub(super) fn frame_runtime<'a>(
        &'a self,
        inferior: &'a Inferior,
        pid: Pid,
        frame: &'a ResolvedFrame,
        module: &'a RuntimeModule,
    ) -> LinuxVariableRuntime<'a, P> {
        LinuxVariableRuntime {
            ptrace: &self.ptrace,
            pid,
            loaded_module: module.loaded,
            breakpoints: &inferior.breakpoints,
            registers: &frame.registers,
            floating: None,
            cfa: frame.cfa.clone(),
            link_map: module.link_map,
        }
    }

    pub(super) fn variables(
        &self,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
        query: &VariableQuery,
        limits: crate::InspectionLimits,
    ) -> Result<VariableSnapshot> {
        validate_inspection_limits(limits)?;
        let mut budget = InspectionBudget::new(limits);
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_stopped_thread(inferior, pid)?;
        // After exec(2) the retained module catalog and image metadata describe
        // the previous program, but the stopped thread now executes the new
        // image. Resolving a variable against stale metadata would silently
        // produce a convincing but incorrect value, so refuse inspection in the
        // exec-replaced state exactly as run control does.
        validate_image_current(inferior)?;
        // Source-level visibility follows the frame's logical scope: an inline
        // frame scopes lookup to that instance's variables, a physical frame
        // to the containing function's own variables.
        let resolved = self.resolve_frame(inferior, pid, frame)?;
        let scope = self.frame_scope(&resolved);
        let inspect_locals = |budget: &mut InspectionBudget| {
            let (module, address, selected) = scope.ok_or(Error::VariableContextUnsupported)?;
            let mut runtime = self.frame_runtime(inferior, pid, &resolved, module);
            module.variables.inspect(
                address,
                selected,
                query,
                variable_context(stop_id, pid, frame, module, Some(address)),
                &mut runtime,
                budget,
            )
        };
        let variables = match query {
            VariableQuery::Global(global) => {
                vec![self.inspect_loaded_global(inferior, pid, &resolved, *global, &mut budget)?]
            }
            VariableQuery::All => inspect_locals(&mut budget)?,
            VariableQuery::Name(name) => {
                let local = if scope.is_some() {
                    inspect_locals(&mut budget)
                } else {
                    Err(Error::VariableNotFound(name.clone()))
                };
                match local {
                    Ok(variables) => variables,
                    Err(Error::VariableNotFound(_)) => {
                        let mut matches = Vec::new();
                        for module in self.modules.values() {
                            match module.image.global_named(name) {
                                Ok(global) => matches.push(GlobalVariableReference {
                                    module: module.loaded.id,
                                    image: module.loaded.image,
                                    variable: global.id,
                                }),
                                Err(Error::VariableNotFound(_)) => {}
                                Err(error) => return Err(error),
                            }
                        }
                        let [global] = matches.as_slice() else {
                            if matches.is_empty() {
                                return Err(Error::VariableNotFound(name.clone()));
                            }
                            return Err(Error::AmbiguousLoadedGlobalVariable {
                                selector: name.clone(),
                                candidates: matches,
                            });
                        };
                        vec![self.inspect_loaded_global(
                            inferior,
                            pid,
                            &resolved,
                            *global,
                            &mut budget,
                        )?]
                    }
                    Err(error) => return Err(error),
                }
            }
        };
        Ok(VariableSnapshot {
            revision: self.revision,
            stop_id,
            thread: debug_thread_id(pid),
            stack_frame: frame,
            frame: resolved.presented,
            target: self.module_image.target(),
            variables: variables.into(),
            completion: budget.completion(),
            usage: budget.usage(),
        })
    }

    pub(super) fn inspect_loaded_global(
        &self,
        inferior: &Inferior,
        pid: Pid,
        frame: &ResolvedFrame,
        global: GlobalVariableReference,
        budget: &mut InspectionBudget,
    ) -> Result<crate::Variable> {
        let module = self
            .modules
            .get(&global.module)
            .ok_or(Error::ModuleNotLoaded(global.module))?;
        if module.loaded.image != global.image {
            return Err(Error::StaleModuleImage);
        }
        let context_address = global_context_address(frame, module);
        let mut runtime = self.frame_runtime(inferior, pid, frame, module);
        let mut variable = module.variables.inspect_global(
            global.variable,
            context_address,
            variable_context(
                public_stop_id(inferior),
                pid,
                frame.id,
                module,
                context_address,
            ),
            &mut runtime,
            budget,
        )?;
        variable.global = Some(global);
        Ok(variable)
    }

    pub(super) fn inspect(
        &self,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
        expression: &ValueExpression,
        limits: crate::InspectionLimits,
    ) -> Result<InspectedValue> {
        validate_inspection_limits(limits)?;
        let mut budget = InspectionBudget::new(limits);
        self.inspect_with_budget(stop_id, pid, frame, expression, &mut budget)
    }

    pub(super) fn inspect_with_budget(
        &self,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
        expression: &ValueExpression,
        budget: &mut InspectionBudget,
    ) -> Result<InspectedValue> {
        self.inspect_with_root(stop_id, pid, frame, expression, budget)
            .map(|(value, _)| value)
    }

    /// Inspects an expression and reports which data object its longest
    /// matching name prefix resolved to.
    pub(super) fn inspect_with_root(
        &self,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
        expression: &ValueExpression,
        budget: &mut InspectionBudget,
    ) -> Result<(InspectedValue, ExpressionRoot)> {
        validate_value_expression(expression)?;
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_stopped_thread(inferior, pid)?;
        validate_image_current(inferior)?;
        let resolved = self.resolve_frame(inferior, pid, frame)?;
        self.inspect_in_frame(inferior, stop_id, pid, &resolved, expression, budget)
    }

    /// Inspects an expression where a breakpoint hit stopped one thread
    /// while others may run, as a condition or log message does. Its
    /// capabilities belong to no stop.
    pub(super) fn inspect_at_hit(
        &self,
        pid: Pid,
        expression: &ValueExpression,
    ) -> Result<InspectedValue> {
        validate_value_expression(expression)?;
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        // The thread is in the ptrace-stop that reported the hit, which its
        // recorded state does not reflect until the hit is resolved.
        validate_image_current(inferior)?;
        let mut budget = InspectionBudget::new(crate::InspectionLimits::default());
        let address = crate::VirtualAddress::new(self.ptrace.registers(pid)?.rip);
        let presentation = self.presentation_for_thread(
            pid,
            Some(&crate::StopReason::Breakpoint {
                address,
                hits: Arc::from([]),
            }),
        )?;
        let resolved =
            self.resolve_presented_frame(inferior, pid, StackFrameId::INNERMOST, presentation)?;
        self.inspect_in_frame(
            inferior,
            StopId::new(0),
            pid,
            &resolved,
            expression,
            &mut budget,
        )
        .map(|(value, _)| value)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "longest-prefix local/global lookup shares one validated stopped runtime"
    )]
    fn inspect_in_frame(
        &self,
        inferior: &Inferior,
        stop_id: StopId,
        pid: Pid,
        resolved: &ResolvedFrame,
        expression: &ValueExpression,
        budget: &mut InspectionBudget,
    ) -> Result<(InspectedValue, ExpressionRoot)> {
        let frame = resolved.id;
        let scope = self.frame_scope(resolved);
        let mut runtime =
            scope.map(|(module, ..)| self.frame_runtime(inferior, pid, resolved, module));

        let named_prefix = expression
            .steps
            .iter()
            .take_while(|step| matches!(step, ValuePathStep::Named(_)))
            .count();
        for root_components in (1..=named_prefix).rev() {
            let root = expression.steps[..root_components]
                .iter()
                .map(|step| match step {
                    ValuePathStep::Named(name) => name.as_str(),
                    _ => unreachable!("root prefix contains only names"),
                })
                .collect::<Vec<_>>()
                .join(".");
            let selectors = &expression.steps[root_components..];
            let local = match (scope, runtime.as_mut()) {
                (Some((module, address, selected)), Some(runtime)) => module
                    .variables
                    .visible_object(address, selected, &root)
                    .and_then(|object| {
                        crate::debug_info::inspect_path(
                            module.variables.as_ref(),
                            object,
                            selectors,
                            Some(address),
                            variable_context(stop_id, pid, frame, module, Some(address)),
                            runtime,
                            budget,
                        )
                    })
                    .map(|value| {
                        (
                            value,
                            ExpressionRootKind::Local {
                                name: root.clone(),
                                module: module.loaded.id,
                                address,
                                selected,
                            },
                        )
                    }),
                _ => Err(Error::VariableNotFound(root.clone())),
            };
            match local {
                Ok((value, kind)) => {
                    return Ok((
                        value,
                        ExpressionRoot {
                            components: root_components,
                            kind,
                        },
                    ));
                }
                Err(Error::VariableNotFound(_)) => {}
                Err(error) => return Err(error),
            }

            let mut matches = Vec::new();
            for module in self.modules.values() {
                match module.image.global_named(&root) {
                    Ok(global) => matches.push(GlobalVariableReference {
                        module: module.loaded.id,
                        image: module.loaded.image,
                        variable: global.id,
                    }),
                    Err(Error::VariableNotFound(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            match matches.as_slice() {
                [] => {}
                [global] => {
                    return self
                        .inspect_loaded_global_path(
                            inferior, pid, resolved, *global, selectors, budget,
                        )
                        .map(|value| {
                            (
                                value,
                                ExpressionRoot {
                                    components: root_components,
                                    kind: ExpressionRootKind::Global(*global),
                                },
                            )
                        });
                }
                _ => {
                    return Err(Error::AmbiguousLoadedGlobalVariable {
                        selector: root,
                        candidates: matches,
                    });
                }
            }
        }

        Err(Error::VariableNotFound(
            expression
                .steps
                .iter()
                .filter_map(|step| match step {
                    ValuePathStep::Named(name) => Some(name.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("."),
        ))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "range validation and selection preserve one atomic inspection budget"
    )]
    pub(super) fn inspect_range(
        &self,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
        expression: &ValueExpression,
        range: ValueIndexRange,
        limits: crate::InspectionLimits,
    ) -> Result<crate::ValueChildPage> {
        validate_inspection_limits(limits)?;
        let mut budget = InspectionBudget::new(limits);
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_stopped_thread(inferior, pid)?;
        let length = range
            .end
            .checked_sub(range.start)
            .ok_or_else(|| Error::InvalidValueRange("the range end precedes its start".into()))?;
        if length < 0 {
            return Err(Error::InvalidValueRange(
                "the range end precedes its start".into(),
            ));
        }
        if length > i128::from(MAX_VALUE_CHILD_PAGE_LIMIT) {
            return Err(Error::InvalidValueRange(
                format!("a range may contain at most {MAX_VALUE_CHILD_PAGE_LIMIT} elements").into(),
            ));
        }
        let inspected = self.inspect_with_budget(stop_id, pid, frame, expression, &mut budget)?;
        let type_name = inspected
            .type_info
            .as_ref()
            .map_or_else(|| Arc::from("<unknown>"), |info| Arc::clone(&info.name));
        let (lower_bound, count, reference) = match inspected.state {
            crate::VariableState::Available {
                value: crate::VariableValue::Array { ref dimensions, .. },
                children: crate::ValueChildren::Available(reference),
                ..
            } => {
                let [dimension] = dimensions.as_ref() else {
                    return Err(Error::InvalidValueRange(
                        "ranges currently require a one-dimensional array".into(),
                    ));
                };
                (dimension.lower_bound, dimension.count, reference)
            }
            crate::VariableState::Available {
                value: crate::VariableValue::Slice { length, .. },
                children: crate::ValueChildren::Available(reference),
                ..
            } => (0, length, reference),
            crate::VariableState::Available { .. } => {
                return Err(Error::IndexAccessOnNonIndexable { type_name });
            }
            crate::VariableState::Unavailable(VariableUnavailableReason::InspectionLimit(_)) => {
                return Ok(crate::ValueChildPage {
                    stop_id,
                    offset: 0,
                    total: 0,
                    children: Arc::from([]),
                    completion: budget.completion(),
                    usage: budget.usage(),
                });
            }
            crate::VariableState::Unavailable(reason) => {
                return Err(Error::InvalidValueRange(
                    format!("the selected aggregate is unavailable: {reason}").into(),
                ));
            }
            crate::VariableState::Malformed(reason) => {
                return Err(Error::InvalidValueRange(
                    format!(
                        "the selected aggregate is malformed: {}",
                        reason.description
                    )
                    .into(),
                ));
            }
            crate::VariableState::Invalid { reason, .. } => {
                return Err(Error::InvalidValueRange(
                    format!("the selected aggregate has an invalid value: {reason}").into(),
                ));
            }
        };
        let relative_start = range
            .start
            .checked_sub(lower_bound)
            .and_then(|index| u64::try_from(index).ok());
        let relative_end = range
            .end
            .checked_sub(lower_bound)
            .and_then(|index| u64::try_from(index).ok());
        let (Some(offset), Some(end)) = (relative_start, relative_end) else {
            return Err(Error::ValueIndexOutOfBounds {
                index: range.start,
                lower_bound,
                count,
            });
        };
        if offset > count || end > count {
            return Err(Error::ValueIndexOutOfBounds {
                index: if offset > count {
                    range.start
                } else {
                    range.end.checked_sub(1).unwrap_or(range.end)
                },
                lower_bound,
                count,
            });
        }
        if length == 0 {
            return Ok(crate::ValueChildPage {
                stop_id,
                offset,
                total: count,
                children: Arc::from([]),
                completion: budget.completion(),
                usage: budget.usage(),
            });
        }
        self.value_children_with_budget(
            &reference,
            &crate::ValueChildQuery {
                offset,
                limit: u32::try_from(length).expect("validated range length fits u32"),
            },
            &mut budget,
        )
    }

    pub(super) fn inspect_loaded_global_path(
        &self,
        inferior: &Inferior,
        pid: Pid,
        frame: &ResolvedFrame,
        global: GlobalVariableReference,
        selectors: &[ValuePathStep],
        budget: &mut InspectionBudget,
    ) -> Result<InspectedValue> {
        let module = self
            .modules
            .get(&global.module)
            .ok_or(Error::ModuleNotLoaded(global.module))?;
        if module.loaded.image != global.image {
            return Err(Error::StaleModuleImage);
        }
        let context_address = global_context_address(frame, module);
        let mut runtime = self.frame_runtime(inferior, pid, frame, module);
        let object = module.variables.global_object(global.variable)?;
        crate::debug_info::inspect_path(
            module.variables.as_ref(),
            object,
            selectors,
            context_address,
            variable_context(
                public_stop_id(inferior),
                pid,
                frame.id,
                module,
                context_address,
            ),
            &mut runtime,
            budget,
        )
    }

    pub(super) fn dereference(
        &self,
        reference: &crate::DereferenceReference,
        limits: crate::InspectionLimits,
    ) -> Result<crate::DereferencedValue> {
        validate_inspection_limits(limits)?;
        let mut budget = InspectionBudget::new(limits);
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        // Validate the stop before consulting modules, registers, or memory.
        validate_public_stop(inferior, Some(reference.stop_id))?;
        let pid = debug_pid(reference.thread)?;
        validate_stopped_thread(inferior, pid)?;
        validate_image_current(inferior)?;
        let module = self
            .modules
            .get(&reference.module)
            .ok_or(Error::ModuleNotLoaded(reference.module))?;
        if module.loaded.image != reference.image {
            return Err(Error::StaleModuleImage);
        }
        // The capability evaluates in the frame that produced it, whichever
        // frame is selected now.
        let frame = self.resolve_frame(inferior, pid, reference.frame)?;
        let mut runtime = self.frame_runtime(inferior, pid, &frame, module);
        module
            .variables
            .dereference(reference, &mut runtime, &mut budget)
    }

    pub(super) fn value_children(
        &self,
        reference: &crate::ValueChildrenReference,
        query: &crate::ValueChildQuery,
        limits: crate::InspectionLimits,
    ) -> Result<crate::ValueChildPage> {
        validate_inspection_limits(limits)?;
        let mut budget = InspectionBudget::new(limits);
        self.value_children_with_budget(reference, query, &mut budget)
    }

    pub(super) fn value_children_with_budget(
        &self,
        reference: &crate::ValueChildrenReference,
        query: &crate::ValueChildQuery,
        budget: &mut InspectionBudget,
    ) -> Result<crate::ValueChildPage> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        // A capability must be rejected before consulting modules, registers,
        // or memory if its stopped snapshot is no longer current.
        validate_public_stop(inferior, Some(reference.stop_id))?;
        if !(1..=MAX_VALUE_CHILD_PAGE_LIMIT).contains(&query.limit) {
            return Err(Error::InvalidValueChildPageLimit(query.limit));
        }
        let pid = debug_pid(reference.thread)?;
        validate_stopped_thread(inferior, pid)?;
        validate_image_current(inferior)?;
        let module = self
            .modules
            .get(&reference.module)
            .ok_or(Error::ModuleNotLoaded(reference.module))?;
        if module.loaded.image != reference.image {
            return Err(Error::StaleModuleImage);
        }
        let frame = self.resolve_frame(inferior, pid, reference.frame)?;
        let mut runtime = self.frame_runtime(inferior, pid, &frame, module);
        module
            .variables
            .value_children(reference, query.offset, query.limit, &mut runtime, budget)
    }

    pub(super) fn globals(&self, query: &GlobalVariableQuery) -> Result<GlobalVariablePage> {
        if !(1..=256).contains(&query.limit) {
            return Err(Error::InvalidGlobalPageLimit(query.limit));
        }
        let running = self.inferior.is_some();
        let mut matches = self
            .modules
            .values()
            .flat_map(|module| {
                module
                    .image
                    .globals()
                    .iter()
                    .filter(|global| {
                        query.filter.as_ref().is_none_or(|filter| {
                            global.name.contains(filter)
                                || global.qualified_name.contains(filter)
                                || global
                                    .linkage_name
                                    .as_ref()
                                    .is_some_and(|linkage| linkage.contains(filter))
                        })
                    })
                    .map(|global| LoadedGlobalVariableInfo {
                        module: running.then_some(module.loaded),
                        image: module.image.id(),
                        variable: global.clone(),
                    })
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            left.variable
                .qualified_name
                .cmp(&right.variable.qualified_name)
                .then_with(|| left.image.cmp(&right.image))
                .then_with(|| left.variable.id.cmp(&right.variable.id))
        });
        let total = u64::try_from(matches.len()).expect("global count fits u64");
        let start = usize::try_from(query.offset)
            .unwrap_or(usize::MAX)
            .min(matches.len());
        let end = start
            .saturating_add(usize::try_from(query.limit).expect("u32 fits usize"))
            .min(matches.len());
        let variables = matches[start..end].to_vec().into();
        Ok(GlobalVariablePage {
            revision: self.revision,
            offset: query.offset,
            total,
            variables,
        })
    }
}

fn variable_context(
    stop_id: StopId,
    pid: Pid,
    frame: StackFrameId,
    module: &RuntimeModule,
    address: Option<ImageAddress>,
) -> VariableContext {
    VariableContext {
        stop_id,
        thread: debug_thread_id(pid),
        frame,
        module: module.loaded.id,
        image: module.loaded.image,
        address,
    }
}

/// The stop whose capabilities inspection hands out: the public stop, or
/// none at all for values read at a breakpoint hit.
fn public_stop_id(inferior: &Inferior) -> StopId {
    inferior
        .public_stop
        .as_ref()
        .map_or_else(|| StopId::new(0), |stop| stop.id)
}

/// The instruction context that selects a global's range-gated location
/// entries. A frame executing another module gives none, so the provider
/// refuses to guess rather than resolving against an unrelated address.
fn global_context_address(frame: &ResolvedFrame, module: &RuntimeModule) -> Option<ImageAddress> {
    frame
        .code
        .filter(|(code_module, _)| *code_module == module.loaded.id)
        .map(|(_, address)| address)
}

pub(super) fn variable_cfa_error(termination: &UnwindTermination) -> VariableRuntimeError {
    match termination {
        // Defective unwind metadata makes a value malformed; anything else
        // only leaves the call-frame address unavailable.
        UnwindTermination::CorruptUnwindInfo { .. }
        | UnwindTermination::InvalidCaller { .. }
        | UnwindTermination::CycleDetected => {
            VariableRuntimeError::Malformed(termination.to_string().into())
        }
        _ => VariableUnavailableReason::CallFrameUnavailable(
            CallFrameUnavailableReason::UnwindTerminated(termination.to_string().into()),
        )
        .into(),
    }
}

pub(super) struct LinuxVariableRuntime<'a, P> {
    pub(super) ptrace: &'a P,
    pub(super) pid: Pid,
    pub(super) loaded_module: LoadedModule,
    pub(super) breakpoints: &'a BTreeMap<VirtualAddress, BreakpointSite>,
    pub(super) registers: &'a FrameRegisters,
    pub(super) floating: Option<std::result::Result<Fxsave, Arc<str>>>,
    pub(super) cfa: std::result::Result<VirtualAddress, VariableRuntimeError>,
    pub(super) link_map: Option<VirtualAddress>,
}

impl<P: InspectionOps> VariableRuntime for LinuxVariableRuntime<'_, P> {
    fn register(
        &mut self,
        register: u16,
    ) -> std::result::Result<VariableRegister, VariableRuntimeError> {
        let native = match self.registers {
            FrameRegisters::Thread(native) => native,
            FrameRegisters::Caller(registers) => {
                return x86_64_caller_variable_register(registers, register);
            }
        };
        if let Some(value) = x86_64_general_variable_register(native, register) {
            return Ok(value);
        }
        if (17..=32).contains(&register) {
            let floating = self.floating.get_or_insert_with(|| {
                self.ptrace
                    .floating_registers(self.pid)
                    .map_err(|error| Arc::from(error.to_string()))
            });
            return floating
                .as_ref()
                .map_err(|error| {
                    VariableRuntimeError::Unavailable(
                        VariableUnavailableReason::RegisterUnavailable(
                            format!("xmm{} ({error})", register - 17).into(),
                        ),
                    )
                })
                .map(|floating| x86_64_xmm_variable_register(floating, register));
        }
        Err(VariableRuntimeError::Unavailable(
            crate::UnsupportedVariableFeature::RegisterClass.into(),
        ))
    }

    fn call_frame_cfa(&self) -> std::result::Result<VirtualAddress, VariableRuntimeError> {
        self.cfa.clone()
    }

    fn tls_address(
        &mut self,
        offset: u64,
    ) -> std::result::Result<VirtualAddress, VariableUnavailableReason> {
        let link_map = self
            .link_map
            .ok_or(VariableUnavailableReason::TlsUnavailable(
                TlsUnavailableReason::ModuleIdentityUnavailable,
            ))?;
        self.ptrace
            .tls_address(self.pid, link_map, offset)
            .map_err(|reason| {
                VariableUnavailableReason::TlsUnavailable(TlsUnavailableReason::LookupFailed(
                    reason,
                ))
            })
    }

    fn relocate(&self, address: ImageAddress) -> std::result::Result<VirtualAddress, Arc<str>> {
        self.loaded_module
            .virtual_address(address)
            .map_err(|error| error.to_string().into())
    }

    fn read_memory(
        &mut self,
        address: VirtualAddress,
        size: usize,
    ) -> std::result::Result<Arc<[u8]>, VariableRuntimeError> {
        let read = read_logical_memory(self.ptrace, self.pid, self.breakpoints, address, size)
            .map_err(|error| match error {
                // A location computed from a meaningless frame base, as
                // before a prologue, can wrap the address space: that one
                // value is unavailable.
                Error::AddressOverflow => {
                    VariableRuntimeError::Unavailable(VariableUnavailableReason::ValueAccess(
                        crate::ValueAccessUnavailableReason::AddressOverflow,
                    ))
                }
                error => VariableRuntimeError::Fatal(error.to_string().into()),
            })?;
        match read.completion {
            MemoryReadCompletion::Complete => Ok(Arc::from(read.bytes)),
            MemoryReadCompletion::Incomplete { next_address, .. } => Err(
                VariableRuntimeError::Unavailable(VariableUnavailableReason::MemoryInaccessible {
                    address,
                    requested: u64::try_from(size).unwrap_or(u64::MAX),
                    completed: u64::try_from(read.bytes.len()).unwrap_or(u64::MAX),
                    next_address,
                }),
            ),
        }
    }
}

pub(super) fn validate_value_expression(expression: &ValueExpression) -> Result<()> {
    if expression.steps.is_empty()
        || !matches!(expression.steps.first(), Some(ValuePathStep::Named(_)))
    {
        return Err(Error::InvalidValueExpression(
            "an expression must name a data object".to_owned(),
        ));
    }
    if expression.steps.len() > MAX_VALUE_EXPRESSION_STEPS {
        return Err(Error::InvalidValueExpression(format!(
            "an expression may contain at most {MAX_VALUE_EXPRESSION_STEPS} operations"
        )));
    }
    if expression
        .steps
        .iter()
        .any(|step| matches!(step, ValuePathStep::Named(name) if name.is_empty()))
    {
        return Err(Error::InvalidValueExpression(
            "expression names must not be empty".to_owned(),
        ));
    }
    if expression
        .steps
        .iter()
        .filter(|step| matches!(step, ValuePathStep::Dereference))
        .count()
        > MAX_VALUE_EXPRESSION_DEREFERENCES
    {
        return Err(Error::InvalidValueExpression(format!(
            "an expression may contain at most {MAX_VALUE_EXPRESSION_DEREFERENCES} explicit dereferences"
        )));
    }
    Ok(())
}

pub(super) fn validate_inspection_limits(limits: crate::InspectionLimits) -> Result<()> {
    for (resource, value, maximum) in [
        (
            crate::InspectionLimit::Variables,
            limits.variables,
            MAX_INSPECTION_LIMITS.variables,
        ),
        (
            crate::InspectionLimit::ValueNodes,
            limits.value_nodes,
            MAX_INSPECTION_LIMITS.value_nodes,
        ),
        (
            crate::InspectionLimit::AggregateDepth,
            limits.aggregate_depth,
            MAX_INSPECTION_LIMITS.aggregate_depth,
        ),
        (
            crate::InspectionLimit::MemoryReads,
            limits.memory_reads,
            MAX_INSPECTION_LIMITS.memory_reads,
        ),
        (
            crate::InspectionLimit::MemoryBytes,
            limits.memory_bytes,
            MAX_INSPECTION_LIMITS.memory_bytes,
        ),
        (
            crate::InspectionLimit::ExpressionWork,
            limits.expression_work,
            MAX_INSPECTION_LIMITS.expression_work,
        ),
    ] {
        if value == 0 || value > maximum {
            return Err(Error::InvalidInspectionLimit {
                resource,
                value,
                maximum,
            });
        }
    }
    Ok(())
}
