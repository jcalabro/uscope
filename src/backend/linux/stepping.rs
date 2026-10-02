//! Source and instruction stepping plans and their completion rules.

use std::collections::BTreeSet;

use nix::libc;
use nix::unistd::Pid;

use crate::protocol::{
    DebuggerEvent, ExecutionId, PresentedFrame, ProcessId, StepKind, StopId, StopReason,
};
use crate::unwind::{CallerProvider, CallerResult, DEFAULT_MAX_FRAMES, FrameContext};
use crate::{
    CodeInstanceKind, Error, ImageLocation, InlineFrameLookup, Result, StackFrameId, VirtualAddress,
};

use super::breakpoints::install_plan_breakpoint;
use super::frames::{
    DwarfCallerProvider, code_instance_is_active, frame_lookup_address, make_presentation,
    presentation_visible_count, selected_code_instance, source_for_code_instance,
    source_line_changed, source_step_destination,
};
use super::memory::PtraceMemory;
use super::native::LinuxTraceOps;
use super::registers::x86_64_registers;
use crate::disassembly::{AssemblySyntax, ControlFlow, RawDecode, decoder_for};

use super::memory::read_logical_memory;
use super::{
    ActiveKind, BreakpointOwner, Controller, EpilogueTraversal, ExpectedStop, Inferior, LinuxError,
    NativeThreadState, Resume, ReturnTraversal, StepStart, allocate_stop_id, backend_error,
    debug_thread_id, process_id, steps_instructions, validate_process, validate_public_stop,
    validate_resumable, validate_stopped_thread,
};

impl<P: LinuxTraceOps> Controller<P> {
    /// Steps into an inline frame hidden at the current instruction without
    /// running the inferior. On success returns the execution and the stop
    /// event to publish once the step is acknowledged.
    pub(super) fn try_virtual_step(
        &mut self,
        requested_process: ProcessId,
        stop_id: StopId,
        pid: Pid,
        kind: StepKind,
    ) -> Result<Option<(ExecutionId, DebuggerEvent)>> {
        if kind != StepKind::IntoSource {
            return Ok(None);
        }

        let (process_id, execution_id, next_stop_id, presentation) = {
            let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
            validate_process(inferior, requested_process)?;
            validate_public_stop(inferior, Some(stop_id))?;
            validate_resumable(inferior)?;
            validate_stopped_thread(inferior, pid)?;
            let stop = inferior
                .public_stop
                .as_ref()
                .expect("public stop was validated");
            let Some(current) = stop.presentations.get(&pid) else {
                return Ok(None);
            };
            if current.hidden_inline_frames == 0
                || matches!(current.frame, PresentedFrame::Ambiguous(_))
            {
                return Ok(None);
            }

            let image_address = inferior.loaded_module.image_address(current.instruction)?;
            let location = self.module_image.locate(image_address);
            let InlineFrameLookup::Unique(chain) = &location.inline_frames else {
                return Err(Error::AmbiguousInlineFrame);
            };
            let visible = presentation_visible_count(&location, current)?;
            let presentation =
                make_presentation(current.instruction, chain.instances.as_ref(), visible + 1)?;

            (
                process_id(inferior.tgid),
                ExecutionId::new(inferior.next_execution.wrapping_add(1)),
                allocate_stop_id(),
                presentation,
            )
        };

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.next_execution = execution_id.get();
        let stop = inferior
            .public_stop
            .as_mut()
            .expect("public stop was validated");
        stop.id = next_stop_id;
        stop.triggering_thread = pid;
        stop.selected_frames.clear();
        stop.reason = StopReason::Step { kind };
        stop.presentations.insert(pid, presentation);
        inferior.thread_mut(pid)?.reason = Some(StopReason::Step { kind });
        inferior.selected_thread = Some(pid);

        self.bump_revision();
        let stopped = DebuggerEvent::InferiorStopped {
            revision: self.revision,
            process_id,
            execution_id: Some(execution_id),
            stop_id: next_stop_id,
            thread_id: debug_thread_id(pid),
            reason: StopReason::Step { kind },
        };
        Ok(Some((execution_id, stopped)))
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn start_user_step(&mut self, pid: Pid, kind: StepKind) -> Result<()> {
        let uses_plan_breakpoints = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .is_some_and(|active| {
                matches!(
                    &active.kind,
                    ActiveKind::Step { start, .. }
                        if !start.plan_addresses.is_empty() || start.signal_guard.is_some()
                )
            });
        if uses_plan_breakpoints {
            return self.continue_thread(pid);
        }
        if kind == StepKind::IntoSource
            && self.stopped_outside_described_code(pid)?
            && self.escape_undescribed_code(pid)?
        {
            return Ok(());
        }

        self.resume_native(pid, Resume::Step, true, ExpectedStop::UserStep { kind })
    }

    /// Reports whether the thread is stopped at an instruction that no DWARF
    /// code instance describes (PLT stubs, library code, assembly thunks).
    pub(super) fn stopped_outside_described_code(&self, pid: Pid) -> Result<bool> {
        let registers = self.ptrace.registers(pid)?;
        Ok(self
            .image_location(VirtualAddress::new(registers.rip))
            .is_none_or(|location| {
                location.physical_instance.is_none() && location.source.is_none()
            }))
    }

    /// Runs to the caller instead of instruction-stepping through code without
    /// debug information. The return address comes from call-frame information
    /// when it covers the stopped address (PLT stubs), otherwise from the top
    /// of the stack, which holds the return address immediately after the call
    /// that entered the undescribed code. Either candidate is trusted only
    /// when it resolves to a described instruction. Returns false when no
    /// trustworthy return address exists and the caller should fall back to
    /// instruction stepping.
    pub(super) fn escape_undescribed_code(&mut self, pid: Pid) -> Result<bool> {
        let registers = self.ptrace.registers(pid)?;
        let candidate = match self.caller_address(pid, &registers) {
            Ok(address) => address,
            Err(_) => match self.ptrace.read_word(pid, registers.rsp) {
                Ok(word) => VirtualAddress::new(word),
                Err(_) => return Ok(false),
            },
        };
        let described = self
            .image_location(candidate)
            .is_some_and(|location| location.physical_instance.is_some());
        if !described {
            return Ok(false);
        }
        let execution = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .map(|active| active.id)
            .ok_or(Error::NotRunning)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        install_plan_breakpoint(&self.ptrace, inferior, candidate, execution)?;
        self.continue_thread(pid)?;
        Ok(true)
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn complete_user_step(&mut self, pid: Pid, kind: StepKind) -> Result<()> {
        self.retire_return_guard()?;
        self.retire_epilogue_return_guard()?;
        if !steps_instructions(kind) && self.begin_epilogue_traversal(pid)? {
            return self.start_user_step(pid, kind);
        }
        if self.source_step_returned_to_undescribed_code(pid, kind)? {
            let execution = self
                .inferior
                .as_ref()
                .and_then(|inferior| inferior.active.as_ref())
                .map(|active| active.id)
                .ok_or(Error::NotRunning)?;
            self.cleanup_plan_breakpoints(execution)?;
            return self.continue_thread(pid);
        }
        if matches!(kind, StepKind::OverSource | StepKind::Out) {
            match self.begin_return_traversal(pid) {
                Ok(true) => return self.start_user_step(pid, kind),
                Ok(false) => {}
                // An unavailable unwind (tail call into a shared library, PLT
                // stub, or CFI-less code) is expected lack of evidence, not a
                // controller failure. Stay on the instruction-stepping path.
                Err(Error::Backend(error))
                    if matches!(
                        error.downcast_ref::<LinuxError>(),
                        Some(LinuxError::CallerUnavailable(_))
                    ) =>
                {
                    return self.start_user_step(pid, kind);
                }
                Err(error) => return Err(error),
            }
        }
        if self.step_is_complete(pid, kind)? {
            self.begin_visible_stop(pid, StopReason::Step { kind })
        } else {
            self.start_user_step(pid, kind)
        }
    }

    /// Source stepping has no truthful stop to publish after its starting
    /// activation returns into code for which the debugger has no source.
    /// Keep the operation active so an exit, signal, or user breakpoint is
    /// reported instead of exposing an unusable synthetic source stop.
    pub(super) fn source_step_returned_to_undescribed_code(
        &self,
        pid: Pid,
        kind: StepKind,
    ) -> Result<bool> {
        if !matches!(kind, StepKind::OverSource | StepKind::Out) {
            return Ok(false);
        }
        let activation = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { start, .. } => start.activation,
                _ => None,
            })
            .ok_or(Error::LocationUnavailable)?;
        let registers = self.ptrace.registers(pid)?;
        Ok(x86_64_activation_has_returned(registers.rsp, activation)
            && self
                .image_location(VirtualAddress::new(registers.rip))
                .is_none_or(|location| {
                    location.physical_instance.is_none() && location.source.is_none()
                }))
    }

    /// Turns an exact DWARF `epilogue_begin` row into an internal control site.
    ///
    /// The unwind is performed before the first teardown instruction executes.
    /// Once teardown has begun, the controller relies only on the captured
    /// caller address and caller-side statement breakpoints; it does not make
    /// a convincing but unsafe attempt to unwind a partially destroyed frame.
    pub(super) fn begin_epilogue_traversal(&mut self, pid: Pid) -> Result<bool> {
        let (execution, already_traversing, start_source) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { thread, start, .. } if *thread == pid => Some((
                    active.id,
                    start.epilogue_traversal.is_some() || start.return_traversal.is_some(),
                    start.source.clone(),
                )),
                _ => None,
            })
            .ok_or(Error::NotRunning)?;
        if already_traversing {
            return Ok(false);
        }

        let registers = self.ptrace.registers(pid)?;
        let instruction = VirtualAddress::new(registers.rip);
        let (loaded_module, image_address) = {
            let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
            (
                inferior.loaded_module,
                inferior.loaded_module.image_address(instruction).ok(),
            )
        };
        let Some(image_address) = image_address else {
            return Ok(false);
        };
        let location = self.module_image.locate(image_address);
        if location.physical_instance.is_none()
            || !self
                .module_image
                .control_boundaries_at(image_address)
                .any(|row| row.flags.epilogue_begin())
        {
            return Ok(false);
        }

        // Without unwind evidence for the return, step through the epilogue
        // instruction by instruction instead.
        let Ok(return_address) = self.caller_address(pid, &registers) else {
            return Ok(false);
        };
        let Ok(caller_image) = loaded_module.image_address(return_address) else {
            return Ok(false);
        };
        let caller_location = self.module_image.locate(caller_image);
        let mut completion_addresses = BTreeSet::new();
        if let Some(caller_instance_id) = caller_location.physical_instance
            && let Some(caller_instance) = self.module_image.code_instance(caller_instance_id)
        {
            for line in self.module_image.line_entries() {
                if !line.statement || !caller_instance.contains(line.range.start) {
                    continue;
                }
                if self
                    .module_image
                    .control_boundaries_at(line.range.start)
                    .any(|row| row.flags.epilogue_begin())
                {
                    continue;
                }
                let candidate_location = self.module_image.locate(line.range.start);
                if source_for_code_instance(
                    &self.module_image,
                    &candidate_location,
                    caller_instance_id,
                )
                .is_some_and(|candidate| {
                    source_line_changed(start_source.as_ref(), Some(&candidate))
                }) {
                    completion_addresses.insert(loaded_module.virtual_address(line.range.start)?);
                }
            }
        }

        let mut plan_addresses = completion_addresses.clone();
        plan_addresses.insert(return_address);
        self.install_additional_plan_breakpoints(execution, &plan_addresses)?;

        let start = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.active.as_mut())
            .and_then(|active| match &mut active.kind {
                ActiveKind::Step { start, .. } => Some(start),
                _ => None,
            })
            .expect("source step remained active while installing its epilogue plan");
        start.plan_addresses.extend(plan_addresses);
        start.epilogue_traversal = Some(EpilogueTraversal {
            return_address,
            completion_addresses,
            retire_return_after_repair: false,
        });
        Ok(true)
    }

    /// Runs through a frame that should not become a source-step destination.
    ///
    /// This covers both a sibling call that replaces the starting physical
    /// frame at the same CFA and a regular callee entered while stepping an
    /// inline frame. A return address is safe to use as an internal breakpoint
    /// only when DWARF CFI and the x86-64 System V ABI's `[CFA - 8]` return slot
    /// agree. Failure or disagreement leaves the source operation on its
    /// instruction-stepping path.
    pub(super) fn begin_return_traversal(&mut self, pid: Pid) -> Result<bool> {
        let (execution, already_traversing, activation, start_instance, start_physical) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { thread, start, .. } if *thread == pid => Some((
                    active.id,
                    start.return_traversal.is_some() || start.epilogue_traversal.is_some(),
                    start.activation,
                    start.code_instance,
                    start.physical_instance,
                )),
                _ => None,
            })
            .ok_or(Error::NotRunning)?;
        if already_traversing {
            return Ok(false);
        }
        let (Some(activation), Some(start_instance), Some(start_physical)) =
            (activation, start_instance, start_physical)
        else {
            return Ok(false);
        };

        let registers = self.ptrace.registers(pid)?;
        if x86_64_activation_has_returned(registers.rsp, activation) {
            return Ok(false);
        }
        let Some(starting_activation_location) =
            self.location_for_activation(pid, &registers, activation)?
        else {
            return Ok(false);
        };
        let Ok(current_activation) = self.top_activation(pid, &registers) else {
            return Ok(false);
        };
        let tail_replacement = starting_activation_location
            .physical_instance
            .is_some_and(|current_physical| current_physical != start_physical);
        let selected_is_inline = self
            .module_image
            .code_instance(start_instance)
            .is_some_and(|instance| matches!(instance.kind, CodeInstanceKind::Inline { .. }));
        let entered_nested_callee = selected_is_inline && current_activation < activation;
        let guarded_activation = if tail_replacement {
            activation
        } else if entered_nested_callee {
            current_activation
        } else {
            return Ok(false);
        };

        let Some(return_slot) = guarded_activation.get().checked_sub(8) else {
            return Ok(false);
        };
        let (Ok(cfi_return), Ok(stack_return)) = (
            self.caller_address(pid, &registers),
            self.ptrace.read_word(pid, return_slot),
        ) else {
            return Ok(false);
        };
        let stack_return = VirtualAddress::new(stack_return);
        if cfi_return != stack_return {
            return Ok(false);
        }

        let plan_addresses = BTreeSet::from([cfi_return]);
        self.install_additional_plan_breakpoints(execution, &plan_addresses)?;
        let start = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.active.as_mut())
            .and_then(|active| match &mut active.kind {
                ActiveKind::Step { start, .. } => Some(start),
                _ => None,
            })
            .expect("source step remained active while installing its return plan");
        start.plan_addresses.extend(plan_addresses);
        start.return_traversal = Some(ReturnTraversal {
            return_address: cfi_return,
            guarded_activation,
            retire_return_after_repair: false,
        });
        Ok(true)
    }

    /// Removes the caller guard after its original instruction has been
    /// repaired. If no caller statement breakpoint remains, source stepping
    /// safely falls back to instruction stepping in the now-valid caller.
    pub(super) fn retire_epilogue_return_guard(&mut self) -> Result<()> {
        let retirement = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { start, .. } => start
                    .epilogue_traversal
                    .as_ref()
                    .filter(|traversal| traversal.retire_return_after_repair)
                    .map(|traversal| (active.id, traversal.return_address)),
                _ => None,
            });
        let Some((execution, address)) = retirement else {
            return Ok(());
        };

        self.remove_breakpoint_owner(address, BreakpointOwner::Plan(execution))?;
        let start = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.active.as_mut())
            .and_then(|active| match &mut active.kind {
                ActiveKind::Step { start, .. } => Some(start),
                _ => None,
            })
            .expect("source step remained active while retiring its return guard");
        start.plan_addresses.remove(&address);
        start.epilogue_traversal = None;
        Ok(())
    }

    /// Removes a return guard after its activation reached it without yet
    /// finding a valid source destination. Deeper activations that share a
    /// tail-call guard leave it installed.
    pub(super) fn retire_return_guard(&mut self) -> Result<()> {
        let retirement = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { start, .. } => start
                    .return_traversal
                    .as_ref()
                    .filter(|traversal| traversal.retire_return_after_repair)
                    .map(|traversal| (active.id, traversal.return_address)),
                _ => None,
            });
        let Some((execution, address)) = retirement else {
            return Ok(());
        };

        self.remove_breakpoint_owner(address, BreakpointOwner::Plan(execution))?;
        let start = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.active.as_mut())
            .and_then(|active| match &mut active.kind {
                ActiveKind::Step { start, .. } => Some(start),
                _ => None,
            })
            .expect("source step remained active while retiring its return guard");
        start.plan_addresses.remove(&address);
        start.return_traversal = None;
        Ok(())
    }

    pub(super) fn mark_epilogue_return_for_retirement(&mut self, address: VirtualAddress) {
        let Some(start) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.active.as_mut())
            .and_then(|active| match &mut active.kind {
                ActiveKind::Step { start, .. } => Some(start),
                _ => None,
            })
        else {
            return;
        };
        if let Some(traversal) = start.epilogue_traversal.as_mut()
            && traversal.return_address == address
            && !traversal.completion_addresses.contains(&address)
        {
            traversal.retire_return_after_repair = true;
        }
    }

    pub(super) fn mark_return_guard_for_retirement(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
    ) -> Result<()> {
        let registers = self.ptrace.registers(pid)?;
        let Some(start) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.active.as_mut())
            .and_then(|active| match &mut active.kind {
                ActiveKind::Step { start, .. } => Some(start),
                _ => None,
            })
        else {
            return Ok(());
        };
        if let Some(traversal) = start.return_traversal.as_mut()
            && traversal.return_address == address
            && x86_64_activation_has_returned(registers.rsp, traversal.guarded_activation)
        {
            traversal.retire_return_after_repair = true;
        }
        Ok(())
    }

    pub(super) fn step_is_complete(&self, pid: Pid, kind: StepKind) -> Result<bool> {
        if kind == StepKind::Instruction {
            return Ok(true);
        }
        let registers = self.ptrace.registers(pid)?;
        let start = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match &active.kind {
                ActiveKind::Step { start, .. } => Some(start),
                _ => None,
            })
            .expect("source step has a starting state");
        if kind == StepKind::OverInstruction {
            // A stepped-over call completes when it returns to its caller's
            // stack, not when recursion reaches the same return address.
            return Ok(start.call_return.is_none_or(|(address, stack)| {
                registers.rip == address.get() && registers.rsp == stack
            }));
        }

        if let Some(traversal) = &start.return_traversal {
            let instruction = VirtualAddress::new(registers.rip);
            let stopped_at_breakpoint = self
                .inferior
                .as_ref()
                .and_then(|inferior| inferior.threads.get(&pid))
                .and_then(|thread| thread.stopped_at_breakpoint);
            if stopped_at_breakpoint != Some(instruction) || instruction != traversal.return_address
            {
                return Ok(false);
            }
            if !x86_64_activation_has_returned(registers.rsp, traversal.guarded_activation) {
                return Ok(false);
            }
            if start.activation == Some(traversal.guarded_activation) {
                if kind == StepKind::Out {
                    return Ok(true);
                }
                return Ok(self.image_location(instruction).is_some_and(|location| {
                    source_step_destination(&self.module_image, &location, kind)
                        && source_line_changed(start.source.as_ref(), location.source.as_ref())
                }));
            }
        }

        if let Some(traversal) = &start.epilogue_traversal {
            let instruction = VirtualAddress::new(registers.rip);
            let stopped_at_breakpoint = self
                .inferior
                .as_ref()
                .and_then(|inferior| inferior.threads.get(&pid))
                .and_then(|thread| thread.stopped_at_breakpoint);
            return Ok(stopped_at_breakpoint == Some(instruction)
                && traversal.completion_addresses.contains(&instruction));
        }

        let instruction = VirtualAddress::new(registers.rip);
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let classified_as_breakpoint = inferior
            .threads
            .get(&pid)
            .is_some_and(|thread| thread.stopped_at_breakpoint == Some(instruction));
        if !classified_as_breakpoint
            && inferior
                .breakpoints
                .get(&instruction)
                .is_some_and(|site| site.installed)
        {
            // A trace trap can land immediately before an installed `int3`.
            // Resume once so breakpoint classification and user-over-plan
            // precedence happen through the normal breakpoint path.
            return Ok(false);
        }

        match kind {
            StepKind::Instruction | StepKind::OverInstruction => Ok(true),
            StepKind::IntoSource => self.step_into_source_is_complete(pid, &registers, start),
            StepKind::OverSource | StepKind::Out => {
                let Some(activation) = start.activation else {
                    return Err(Error::LocationUnavailable);
                };
                let Some(code_instance) = start.code_instance else {
                    return Err(Error::LocationUnavailable);
                };
                let Some(location) = self.location_for_activation(pid, &registers, activation)?
                else {
                    return Ok(self.image_location(instruction).is_some_and(|location| {
                        source_step_destination(&self.module_image, &location, kind)
                    }));
                };
                if !code_instance_is_active(&location, code_instance) {
                    // A different physical frame at the same live CFA is a
                    // tail-called replacement, not the caller. Keep stepping
                    // when its return address could not be independently
                    // proven for accelerated traversal.
                    if location.physical_instance != start.physical_instance
                        && !x86_64_activation_has_returned(registers.rsp, activation)
                    {
                        return Ok(false);
                    }
                    return Ok(source_step_destination(&self.module_image, &location, kind));
                }
                let source = source_for_code_instance(&self.module_image, &location, code_instance);

                Ok(kind == StepKind::OverSource
                    && source_step_destination(&self.module_image, &location, kind)
                    && source_line_changed(start.source.as_ref(), source.as_ref()))
            }
        }
    }

    pub(super) fn step_into_source_is_complete(
        &self,
        pid: Pid,
        registers: &libc::user_regs_struct,
        start: &StepStart,
    ) -> Result<bool> {
        let location = self.image_location(VirtualAddress::new(registers.rip));
        // PLT stubs, library code, and other undescribed instructions are not
        // source-step destinations.
        if location.as_ref().is_none_or(|location| {
            location.physical_instance.is_none() && location.source.is_none()
        }) {
            return Ok(false);
        }
        let presentation = self.presentation_for_thread(
            pid,
            Some(&StopReason::Step {
                kind: StepKind::IntoSource,
            }),
        )?;
        let current_instance = location
            .as_ref()
            .map(|location| selected_code_instance(location, &presentation))
            .transpose()?
            .flatten();
        let source = location.as_ref().and_then(|location| {
            current_instance.and_then(|instance| {
                source_for_code_instance(&self.module_image, location, instance)
            })
        });
        let activation = self.top_activation(pid, registers)?;
        let statement = location.as_ref().is_some_and(|location| {
            self.module_image
                .line_entry_containing(location.address)
                .is_some_and(|entry| entry.statement)
        });
        let current_physical = location
            .as_ref()
            .and_then(|location| location.physical_instance);
        let entered_physical_activation = match start.activation {
            Some(start_activation) if start_activation != activation => self
                .location_for_activation(pid, registers, start_activation)?
                .is_some(),
            Some(_) => current_physical.is_some() && current_physical != start.physical_instance,
            None => false,
        };
        // A recommended entry row may carry no source attribution. Source
        // stepping must keep going until it can publish a renderable source
        // stop, so a source-less entry is a waypoint, not a destination.
        let at_recommended_entry = location.as_ref().is_some_and(|location| {
            location.source.is_some()
                && location.physical_instance.is_some_and(|instance| {
                    self.module_image
                        .recommended_entries_for_instance(instance)
                        .any(|entry| entry.address == location.address)
                })
        });
        if entered_physical_activation && !at_recommended_entry {
            return Ok(false);
        }

        Ok(
            (statement || entered_physical_activation && at_recommended_entry)
                && (activation != start.activation.unwrap_or(activation)
                    || current_instance != start.code_instance
                    || source_line_changed(start.source.as_ref(), source.as_ref())),
        )
    }

    pub(super) fn step_start(
        &self,
        pid: Pid,
        kind: StepKind,
        frame: StackFrameId,
    ) -> Result<StepStart> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let thread = inferior.thread(pid)?;
        if !matches!(thread.state, NativeThreadState::Stopped) {
            return Err(Error::NotStopped);
        }
        let registers = self.ptrace.registers(pid)?;
        if frame.get() != 0 {
            return self.outer_step_out_start(pid, &registers, frame);
        }
        let location = self.image_location(VirtualAddress::new(registers.rip));
        let presentation = self.presentation_for_stopped_thread(pid)?;
        let code_instance = location
            .as_ref()
            .map(|location| selected_code_instance(location, &presentation))
            .transpose()?
            .flatten();
        let source = location.as_ref().and_then(|location| {
            code_instance.and_then(|instance| {
                source_for_code_instance(&self.module_image, location, instance)
            })
        });
        // Stepping into source never needs the activation, so a thread
        // stopped in code without unwind information can still step in.
        let activation = match kind {
            StepKind::Instruction | StepKind::OverInstruction => None,
            StepKind::IntoSource => self.top_activation(pid, &registers).ok(),
            StepKind::OverSource | StepKind::Out => Some(self.top_activation(pid, &registers)?),
        };
        let mut plan_addresses = BTreeSet::new();
        let call_return = if kind == StepKind::OverInstruction {
            self.call_return(inferior, pid, &registers)?
        } else {
            None
        };
        plan_addresses.extend(call_return.map(|(address, _)| address));

        let selected_is_inline = code_instance
            .and_then(|instance| self.module_image.code_instance(instance))
            .is_some_and(|instance| matches!(instance.kind, CodeInstanceKind::Inline { .. }));
        // An inline instance has no stack return address of its own. Leaving
        // its physical caller's return address as the only reachable plan
        // breakpoint would run the entire containing activation. Instruction
        // stepping lets `step_is_complete` observe either the next statement
        // in this instance or the point where the logical frame disappears.
        if kind == StepKind::Out && !selected_is_inline {
            plan_addresses.insert(self.caller_address(pid, &registers)?);
        } else if kind == StepKind::OverSource
            && !selected_is_inline
            && let (Some(source), Some(instance_id)) = (&source, code_instance)
            && let Some(instance) = self.module_image.code_instance(instance_id)
        {
            let return_address = self.caller_address(pid, &registers)?;
            for line in self.module_image.line_entries() {
                if !line.statement || !instance.contains(line.range.start) {
                    continue;
                }
                let location = self.module_image.locate(line.range.start);
                if source_for_code_instance(&self.module_image, &location, instance_id)
                    .is_some_and(|candidate| source_line_changed(Some(source), Some(&candidate)))
                {
                    plan_addresses
                        .insert(inferior.loaded_module.virtual_address(line.range.start)?);
                }
            }
            plan_addresses.insert(return_address);
        }

        Ok(StepStart {
            source,
            code_instance,
            physical_instance: location
                .as_ref()
                .and_then(|location| location.physical_instance),
            activation,
            plan_addresses,
            epilogue_traversal: None,
            return_traversal: None,
            signal_guard: None,
            call_return,
        })
    }

    /// Returns the return address and stack pointer of the call instruction
    /// a thread is about to execute, or `None` for any other instruction.
    fn call_return(
        &self,
        inferior: &Inferior,
        pid: Pid,
        registers: &libc::user_regs_struct,
    ) -> Result<Option<(VirtualAddress, u64)>> {
        let mut decoder = decoder_for(self.module_image.target(), AssemblySyntax::Intel)?;
        let read = read_logical_memory(
            &self.ptrace,
            pid,
            &inferior.breakpoints,
            VirtualAddress::new(registers.rip),
            decoder.max_instruction_length(),
        )?;
        Ok(match decoder.decode(registers.rip, &read.bytes, None) {
            RawDecode::Instruction {
                length,
                flow: ControlFlow::Call | ControlFlow::IndirectCall,
                ..
            } => Some((
                VirtualAddress::new(
                    registers
                        .rip
                        .checked_add(u64::try_from(length).expect("instruction length fits u64"))
                        .ok_or(Error::AddressOverflow)?,
                ),
                registers.rsp,
            )),
            _ => None,
        })
    }

    /// Plans running until a frame other than the innermost one returns.
    ///
    /// A logical frame of the innermost activation steps out like the
    /// innermost frame does. An outer activation runs to its return
    /// address, which recursion may reach first from deeper activations;
    /// completion requires the selected activation itself to have returned.
    fn outer_step_out_start(
        &self,
        pid: Pid,
        registers: &libc::user_regs_struct,
        frame: StackFrameId,
    ) -> Result<StepStart> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let resolved = self.resolve_frame(inferior, pid, frame)?;
        let selected = resolved.frame.as_ref().ok_or(Error::AmbiguousInlineFrame)?;
        // Source stepping plans are limited to code the main image describes.
        let Some((_, address)) = resolved
            .code
            .filter(|(module, _)| *module == inferior.loaded_module.id)
        else {
            return Err(Error::FrameStepUnsupported(
                "stepping out applies only to frames of the main executable".into(),
            ));
        };
        let location = self.module_image.locate(address);
        let code_instance = match &resolved.presented {
            PresentedFrame::Inline(instance) => Some(*instance),
            PresentedFrame::Physical => location.physical_instance,
            PresentedFrame::Ambiguous(_) => return Err(Error::AmbiguousInlineFrame),
        }
        .ok_or(Error::LocationUnavailable)?;
        let selected_is_inline = matches!(resolved.presented, PresentedFrame::Inline(_));
        let mut plan_addresses = BTreeSet::new();

        let activation = if resolved.activation == 0 {
            if !selected_is_inline {
                plan_addresses.insert(self.caller_address(pid, registers)?);
            }
            self.top_activation(pid, registers)?
        } else {
            if selected_is_inline {
                return Err(Error::FrameStepUnsupported(
                    "stepping out of an inline frame is supported only in the innermost activation"
                        .into(),
                ));
            }
            let caller_unavailable = |reason| backend_error(LinuxError::CallerUnavailable(reason));
            let activation = resolved.cfa.clone().map_err(|_| {
                caller_unavailable(crate::UnwindTermination::InvalidCaller {
                    description: "the frame's call-frame address is unavailable".into(),
                })
            })?;
            let stack = self.physical_stack(inferior, pid, resolved.activation + 2)?;
            let return_address = stack
                .frames
                .get(resolved.activation + 1)
                .map(|caller| caller.context.instruction)
                .ok_or_else(|| caller_unavailable(stack.termination.clone()))?;
            plan_addresses.insert(return_address);
            activation
        };

        Ok(StepStart {
            source: selected.source.clone(),
            code_instance: Some(code_instance),
            physical_instance: location.physical_instance,
            activation: Some(activation),
            plan_addresses,
            epilogue_traversal: None,
            return_traversal: None,
            signal_guard: None,
            call_return: None,
        })
    }

    pub(super) fn top_activation(
        &self,
        pid: Pid,
        native: &libc::user_regs_struct,
    ) -> Result<VirtualAddress> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let current = FrameContext {
            instruction: VirtualAddress::new(native.rip),
            cfa: None,
            signal_frame: false,
        };
        let mut provider = DwarfCallerProvider {
            modules: vec![self.main_unwind_module(inferior)],
            registers: x86_64_registers(native),
            memory: PtraceMemory {
                ptrace: &self.ptrace,
                pid,
            },
            first: true,
        };

        match provider.caller(&current) {
            CallerResult::Caller(caller) => caller.cfa.ok_or(Error::LocationUnavailable),
            CallerResult::Finished(reason) => {
                Err(backend_error(LinuxError::CallerUnavailable(reason)))
            }
        }
    }

    pub(super) fn location_for_activation(
        &self,
        pid: Pid,
        native: &libc::user_regs_struct,
        activation: VirtualAddress,
    ) -> Result<Option<ImageLocation>> {
        // On x86-64's downward-growing ordinary stack, a live activation's
        // CFA remains above RSP. Once RSP reaches that CFA, the return has
        // already restored the caller's stack. Recognize that transition
        // before asking the main-module-only unwinder to interpret libc code.
        if x86_64_activation_has_returned(native.rsp, activation) {
            return Ok(None);
        }
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let mut context = FrameContext {
            instruction: VirtualAddress::new(native.rip),
            cfa: None,
            signal_frame: false,
        };
        let mut provider = DwarfCallerProvider {
            modules: vec![self.main_unwind_module(inferior)],
            registers: x86_64_registers(native),
            memory: PtraceMemory {
                ptrace: &self.ptrace,
                pid,
            },
            first: true,
        };

        for level in 0..DEFAULT_MAX_FRAMES {
            let caller = match provider.caller(&context) {
                CallerResult::Caller(caller) => caller,
                CallerResult::Finished(reason) => {
                    return Err(backend_error(LinuxError::CallerUnavailable(reason)));
                }
            };
            if caller.cfa == Some(activation) {
                let level = u32::try_from(level).expect("frame limit fits u32");
                let location = frame_lookup_address(level, &context)
                    .and_then(|address| inferior.loaded_module.image_address(address).ok())
                    .filter(|address| self.module_image.contains_address(*address))
                    .map(|address| self.module_image.locate(address));

                return Ok(location);
            }
            // This backend only supports x86-64's downward-growing ordinary stack. Once
            // unwinding passes the starting CFA, that activation has returned.
            if caller.cfa.is_some_and(|cfa| cfa > activation) {
                return Ok(None);
            }
            context = caller;
        }

        Ok(None)
    }

    pub(super) fn caller_address(
        &self,
        pid: Pid,
        native: &libc::user_regs_struct,
    ) -> Result<VirtualAddress> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let current = FrameContext {
            instruction: VirtualAddress::new(native.rip),
            cfa: None,
            signal_frame: false,
        };
        let mut provider = DwarfCallerProvider {
            modules: vec![self.main_unwind_module(inferior)],
            registers: x86_64_registers(native),
            memory: PtraceMemory {
                ptrace: &self.ptrace,
                pid,
            },
            first: true,
        };

        match provider.caller(&current) {
            CallerResult::Caller(caller) => Ok(caller.instruction),
            CallerResult::Finished(reason) => {
                Err(backend_error(LinuxError::CallerUnavailable(reason)))
            }
        }
    }

    pub(super) fn image_location(&self, address: VirtualAddress) -> Option<ImageLocation> {
        let inferior = self.inferior.as_ref()?;
        let image = inferior.loaded_module.image_address(address).ok()?;
        Some(self.module_image.locate(image))
    }
}

pub(super) const fn x86_64_activation_has_returned(
    stack_pointer: u64,
    activation_cfa: VirtualAddress,
) -> bool {
    stack_pointer >= activation_cfa.get()
}
