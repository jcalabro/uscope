//! The values a function returned when a step out finished it. They are
//! read the instant its call returns, where the step stops, as the
//! function's calling convention left them, and kept with the stop, which
//! lists them among the variables of the frame they returned to.

use std::sync::Arc;

use nix::unistd::Pid;

use crate::debug_info::ReturnedValue;
use crate::inspection::InspectionBudget;
use crate::protocol::StopId;
use crate::{
    ImageAddress, ModuleId, Result, StackFrameId, StepKind, StopReason, Variable, VariableKind,
    VirtualAddress,
};

use super::activation::Activation;
use super::frames::{FrameRegisters, StackRoot};
use super::inspection::{LinuxVariableRuntime, variable_context};
use super::native::InspectionOps;
use super::{ActiveKind, Controller, Inferior, RuntimeModule};

/// A function a step out is finishing: where its activation is, where its
/// call returns, and an address of its code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Returning {
    pub(super) activation: Activation,
    pub(super) return_address: VirtualAddress,
    pub(super) function: ImageAddress,
}

/// What a finished function returned, read in the module that describes
/// it.
#[derive(Debug, Clone)]
pub(super) struct Returned {
    module: ModuleId,
    thread: Pid,
    values: Arc<[ReturnedValue]>,
}

impl<P: InspectionOps> Controller<P> {
    /// What the function a step out finished returned, when the step stops
    /// `pid` the instant its call returned: at its return address, with
    /// its activation's stack just popped. A stop anywhere else, as where a
    /// loop's body finished or a panic unwound, returned nothing to show.
    pub(super) fn capture_returned(&self, pid: Pid, reason: &StopReason) -> Option<Returned> {
        if *reason
            != (StopReason::Step {
                kind: StepKind::Out,
            })
        {
            return None;
        }
        let inferior = self.inferior.as_ref()?;
        let ActiveKind::Step { start, .. } = &inferior.active.as_ref()?.kind else {
            return None;
        };
        let returning = start.returning?;
        let registers = self.ptrace.registers(pid).ok()?;
        if VirtualAddress::new(registers.rip) != returning.return_address
            || !returning
                .activation
                .just_returned(self.stack_position(pid, &registers))
        {
            return None;
        }
        let module = self.modules.get(&inferior.loaded_module.id)?;
        let frame = FrameRegisters::Thread(registers);
        let mut runtime = self.thread_runtime(inferior, module, pid, &frame);
        let mut budget = InspectionBudget::new(crate::InspectionLimits::default());
        match module
            .variables
            .returned(returning.function, &mut runtime, &mut budget)
        {
            Ok(Some(values)) => Some(Returned {
                module: module.loaded.id,
                thread: pid,
                values: values.into(),
            }),
            Ok(None) => None,
            Err(error) => {
                record!("the returned values are unreadable: {error}");
                None
            }
        }
    }

    /// The values a step out's function returned, as variables of the
    /// innermost frame of the thread the step stopped, presented at the
    /// stop `stop_id`.
    pub(super) fn returned_variables(
        &self,
        returned: &Returned,
        stop_id: StopId,
        root: &StackRoot,
        frame: StackFrameId,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<Variable>> {
        // The thread, or the task it runs, whose innermost frame the
        // function returned to.
        if frame != StackFrameId::INNERMOST || root.thread() != Some(returned.thread) {
            return Ok(Vec::new());
        }
        let Some(module) = self.modules.get(&returned.module) else {
            return Ok(Vec::new());
        };
        let inferior = self.inferior.as_ref().ok_or(crate::Error::NotRunning)?;
        let registers = FrameRegisters::Thread(self.ptrace.registers(returned.thread)?);
        let mut runtime = self.thread_runtime(inferior, module, returned.thread, &registers);
        let context = variable_context(stop_id, root.context, frame, module, None);
        let mut variables = Vec::with_capacity(returned.values.len());
        for value in returned.values.iter() {
            let (type_info, state) = match &value.value {
                Ok(located) => {
                    let inspected =
                        module
                            .variables
                            .materialize(located, context, &mut runtime, budget)?;
                    (inspected.type_info, inspected.state)
                }
                Err(state) => (
                    value.ty.and_then(|id| {
                        module
                            .image
                            .type_info(crate::TypeReference {
                                image: module.loaded.image,
                                id,
                            })
                            .cloned()
                    }),
                    state.clone(),
                ),
            };
            variables.push(Variable {
                kind: VariableKind::Returned,
                global: None,
                name: Arc::clone(&value.name),
                declaration: None,
                type_info,
                unresolved_shape: value.unresolved_shape.clone(),
                state,
            });
        }
        Ok(variables)
    }

    /// Reads a module's values with a stopped thread's own registers. No
    /// returned value needs a call-frame address: its function's frame is
    /// gone.
    fn thread_runtime<'a>(
        &'a self,
        inferior: &'a Inferior,
        module: &'a RuntimeModule,
        pid: Pid,
        registers: &'a FrameRegisters,
    ) -> LinuxVariableRuntime<'a, P> {
        LinuxVariableRuntime {
            ptrace: &self.ptrace,
            pid,
            thread: Some(pid),
            loaded_module: module.loaded,
            image_range: module.image.address_range(),
            breakpoints: &inferior.breakpoints,
            registers,
            floating: None,
            cfa: Err(crate::debug_info::VariableRuntimeError::Unavailable(
                crate::VariableUnavailableReason::CallFrameUnavailable(
                    crate::CallFrameUnavailableReason::NoInstructionContext,
                ),
            )),
            link_map: module.link_map,
            below_stack_pointer: None,
        }
    }
}
