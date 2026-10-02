//! Stack unwinding and the logical frames presented for inline code.

use std::collections::BTreeSet;

use nix::libc;
use nix::unistd::Pid;

use crate::debug_info::{UnwindInfo, VariableRuntimeError};
use crate::model::FrameMetadata;
use crate::protocol::{FramePresentation, PresentedFrame, StepKind, StopId, StopReason};
use crate::unwind::{
    CallerProvider, CallerResult, DEFAULT_MAX_FRAMES, FrameContext, RegisterFile, collect_frames,
};
use crate::{
    AddressDescription, Backtrace, CallFrameUnavailableReason, CodeInstanceId, CodeInstanceKind,
    Error, ExecutionLocation, FrameKind, ImageAddress, ImageLocation, InlineFrameLookup,
    LoadedModule, ModuleAddress, ModuleId, ModuleImage, Result, SourceLocation, StackFrame,
    StackFrameId, UnwindTermination, VariableUnavailableReason, VirtualAddress,
};

use super::breakpoints::runtime_breakpoint_address;
use super::inspection::variable_cfa_error;
use super::memory::PtraceMemory;
use super::native::InspectionOps;
use super::registers::x86_64_registers;
use super::{
    BreakpointOwner, Controller, Inferior, debug_thread_id, validate_image_current,
    validate_public_stop, validate_stopped_thread,
};

impl<P: InspectionOps> Controller<P> {
    pub(super) fn presentation_for_stopped_thread(&self, pid: Pid) -> Result<FramePresentation> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, inferior.public_stop.as_ref().map(|stop| stop.id))?;
        validate_stopped_thread(inferior, pid)?;
        let stop = inferior
            .public_stop
            .as_ref()
            .expect("public stop was validated");

        // The triggering thread's presentation is cached when the stop is
        // published. Another thread is presented by its own stop reason, or
        // by default when it merely stopped with its siblings.
        stop.presentations.get(&pid).cloned().map_or_else(
            || self.presentation_for_thread(pid, inferior.thread(pid)?.reason.as_ref()),
            Ok,
        )
    }

    /// Chooses the logical frame to present for a stopped thread. `reason` is
    /// the thread's own stop reason, which can reveal or select inline frames.
    pub(super) fn presentation_for_thread(
        &self,
        pid: Pid,
        reason: Option<&StopReason>,
    ) -> Result<FramePresentation> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let registers = self.ptrace.registers(pid)?;
        let instruction = VirtualAddress::new(registers.rip);
        // Presentations describe inline frames of the main image only; an
        // instruction elsewhere, or anywhere after exec replaced the image,
        // is presented as its physical frame.
        let Some(image_address) = inferior
            .loaded_module
            .image_address(instruction)
            .ok()
            .filter(|address| {
                !inferior.exec_unsupported && self.module_image.contains_address(*address)
            })
        else {
            return Ok(FramePresentation {
                instruction,
                frame: PresentedFrame::Physical,
                hidden_inline_frames: 0,
            });
        };
        let location = self.module_image.locate(image_address);

        let InlineFrameLookup::Unique(chain) = &location.inline_frames else {
            return Ok(match &location.inline_frames {
                InlineFrameLookup::Ambiguous(chains) => FramePresentation {
                    instruction,
                    frame: PresentedFrame::Ambiguous(
                        chains
                            .iter()
                            .flat_map(|chain| chain.instances.iter().copied())
                            .collect::<BTreeSet<_>>()
                            .into_iter()
                            .collect::<Vec<_>>()
                            .into(),
                    ),
                    hidden_inline_frames: 0,
                },
                InlineFrameLookup::None => FramePresentation {
                    instruction,
                    frame: PresentedFrame::Physical,
                    hidden_inline_frames: 0,
                },
                InlineFrameLookup::Unique(_) => unreachable!("matched above"),
            });
        };

        let breakpoint_targets = match reason {
            Some(StopReason::Breakpoint { address, .. }) => {
                self.breakpoint_code_instances(inferior, *address)?
            }
            _ => BTreeSet::new(),
        };
        if !breakpoint_targets.is_empty() {
            let active = location
                .physical_instance
                .into_iter()
                .chain(chain.instances.iter().copied())
                .filter(|instance| breakpoint_targets.contains(instance))
                .collect::<Vec<_>>();

            if active.len() > 1 {
                return Ok(FramePresentation {
                    instruction,
                    frame: PresentedFrame::Ambiguous(active.into()),
                    hidden_inline_frames: 0,
                });
            }
            if let Some(target) = active.first().copied() {
                let visible = chain
                    .instances
                    .iter()
                    .position(|instance| *instance == target)
                    .map_or(0, |index| index + 1);

                return make_presentation(instruction, chain.instances.as_ref(), visible);
            }
        }

        let reveal_new_inline = matches!(
            reason,
            Some(StopReason::Step {
                kind: StepKind::IntoSource
            })
        );
        let visible = default_inline_visible_count(
            &self.module_image,
            chain.instances.as_ref(),
            image_address,
            reveal_new_inline,
        );

        make_presentation(instruction, chain.instances.as_ref(), visible)
    }

    pub(super) fn breakpoint_code_instances(
        &self,
        inferior: &Inferior,
        address: VirtualAddress,
    ) -> Result<BTreeSet<CodeInstanceId>> {
        let Some(site) = inferior.breakpoints.get(&address) else {
            return Ok(BTreeSet::new());
        };
        let mut instances = BTreeSet::new();

        for id in site.owners.iter().filter_map(|owner| match owner {
            BreakpointOwner::User(id) => Some(*id),
            BreakpointOwner::Plan(_) => None,
        }) {
            let breakpoint = self
                .breakpoints
                .iter()
                .find(|breakpoint| breakpoint.id == id)
                .expect("physical user owner references a logical breakpoint");
            for resolved in breakpoint.locations.iter() {
                if runtime_breakpoint_address(inferior, resolved.location)? == address {
                    instances.extend(resolved.code_instances.iter().copied());
                }
            }
        }

        Ok(instances)
    }

    pub(super) fn stopped_location(
        &self,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
    ) -> Result<ExecutionLocation> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_stopped_thread(inferior, pid)?;
        validate_image_current(inferior)?;
        if frame.get() != 0 {
            return self.outer_frame_location(inferior, pid, frame);
        }
        let registers = self.ptrace.registers(pid)?;
        let address = VirtualAddress::new(registers.rip);
        let modules = self.unwind_modules(inferior);
        let (module, image_address) =
            unwind_module_for(&modules, address).ok_or(Error::AddressOutsideModule)?;
        let mut image = module.image.locate(image_address);
        // The stop presentation describes inline frames of the main image only.
        if module.loaded.id == inferior.loaded_module.id {
            let presentation = self.presentation_for_stopped_thread(pid)?;
            apply_presentation(&self.module_image, &mut image, &presentation)?;
        }

        Ok(ExecutionLocation {
            module: module.loaded.id,
            address,
            image,
        })
    }

    /// Describes a process address against the modules loaded at a stop.
    pub(super) fn describe_address(
        &self,
        stop_id: StopId,
        address: VirtualAddress,
    ) -> Result<AddressDescription> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_image_current(inferior)?;
        let modules = self.unwind_modules(inferior);
        Ok(describe_address(&modules, address))
    }

    pub(super) fn backtrace(&self, stop_id: StopId, pid: Pid) -> Result<Backtrace> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_stopped_thread(inferior, pid)?;
        validate_image_current(inferior)?;
        let presentation = self.presentation_for_stopped_thread(pid)?;
        let stack = self.physical_stack(inferior, pid, DEFAULT_MAX_FRAMES)?;
        let modules = self.unwind_modules(inferior);

        expand_inline_backtrace(stack.backtrace(pid), &modules, &presentation)
    }

    /// Unwinds at most `max_frames` physical activations of a stopped
    /// thread, keeping the registers the unwinder reconstructed for each.
    pub(super) fn physical_stack(
        &self,
        inferior: &Inferior,
        pid: Pid,
        max_frames: usize,
    ) -> Result<PhysicalStack> {
        let native = self.ptrace.registers(pid)?;
        let initial = FrameContext {
            instruction: VirtualAddress::new(native.rip),
            cfa: None,
            signal_frame: false,
        };
        let mut provider = DwarfCallerProvider {
            modules: self.unwind_modules(inferior),
            registers: x86_64_registers(&native),
            memory: PtraceMemory {
                ptrace: &self.ptrace,
                pid,
            },
            first: true,
        };
        let (frames, termination) = collect_frames(
            initial,
            &mut provider,
            |_, context, provider| PhysicalFrame {
                context: context.clone(),
                registers: provider.registers.clone(),
            },
            max_frames,
        );

        Ok(PhysicalStack {
            native,
            frames,
            termination,
        })
    }

    /// Finds one logical frame of a stopped thread, numbered as
    /// [`Self::backtrace`] presents it, and the state that evaluates its
    /// variables. Only the activations up to that frame are unwound.
    pub(super) fn resolve_frame(
        &self,
        inferior: &Inferior,
        pid: Pid,
        frame: StackFrameId,
    ) -> Result<ResolvedFrame> {
        let presentation = self.presentation_for_stopped_thread(pid)?;
        let level = usize::try_from(frame.get()).expect("u32 fits usize");
        // Every activation presents at least one logical frame, so unwinding
        // one activation per level always reaches the requested frame.
        let max_frames = level.saturating_add(1).min(DEFAULT_MAX_FRAMES);
        let stack = self.physical_stack(inferior, pid, max_frames)?;
        let modules = self.unwind_modules(inferior);

        // An innermost frame without one compatible inline chain still has
        // registers and code, but no single source scope or backtrace frame.
        if level == 0 && matches!(presentation.frame, PresentedFrame::Ambiguous(_)) {
            let innermost = &stack.frames[0];
            let code = unwind_module_for(&modules, innermost.context.instruction)
                .map(|(module, address)| (module.loaded.id, address));
            return Ok(ResolvedFrame {
                id: frame,
                presented: presentation.frame,
                frame: None,
                code,
                scope: FrameScope::Unavailable,
                registers: FrameRegisters::Thread(stack.native),
                cfa: self.frame_cfa(pid, &modules, code, &innermost.registers),
                activation: 0,
            });
        }

        let trace = expand_inline_backtrace(stack.backtrace(pid), &modules, &presentation)?;
        let Some(selected) = trace.frames.get(level).cloned() else {
            return Err(Error::FrameNotFound {
                frame,
                frames: u32::try_from(trace.frames.len()).expect("frame count fits u32"),
            });
        };
        // Inline frames precede the physical frame of their activation.
        let activation = trace.frames[..level]
            .iter()
            .filter(|frame| frame.kind != FrameKind::Inline)
            .count();
        let physical = &stack.frames[activation];
        let code = frame_lookup_address(
            u32::try_from(activation).expect("frame count fits u32"),
            &physical.context,
        )
        .and_then(|lookup| unwind_module_for(&modules, lookup))
        .map(|(module, address)| (module.loaded.id, address));
        let presented = match selected.kind {
            FrameKind::Inline => PresentedFrame::Inline(
                selected
                    .code_instance
                    .expect("inline frames name their code instance"),
            ),
            FrameKind::Physical | FrameKind::Signal => PresentedFrame::Physical,
        };
        // Only code a function describes has a source scope.
        let scope = match (selected.kind, selected.code_instance) {
            (_, None) => FrameScope::Unavailable,
            (FrameKind::Inline, Some(instance)) => FrameScope::Inline(instance),
            (FrameKind::Physical | FrameKind::Signal, Some(_)) => FrameScope::Function,
        };

        Ok(ResolvedFrame {
            id: frame,
            presented,
            frame: Some(selected),
            code,
            scope,
            registers: if activation == 0 {
                FrameRegisters::Thread(stack.native)
            } else {
                FrameRegisters::Caller(physical.registers.clone())
            },
            cfa: self.frame_cfa(pid, &modules, code, &physical.registers),
            activation,
        })
    }

    /// Computes the canonical frame address of the activation executing
    /// `code` with `registers`.
    fn frame_cfa(
        &self,
        pid: Pid,
        modules: &[UnwindModule<'_>],
        code: Option<(ModuleId, ImageAddress)>,
        registers: &RegisterFile,
    ) -> std::result::Result<VirtualAddress, VariableRuntimeError> {
        let Some((module, address)) = code.and_then(|(id, address)| {
            modules
                .iter()
                .find(|module| module.loaded.id == id)
                .map(|module| (module, address))
        }) else {
            return Err(VariableRuntimeError::Unavailable(
                VariableUnavailableReason::CallFrameUnavailable(
                    CallFrameUnavailableReason::NoInstructionContext,
                ),
            ));
        };
        module
            .unwind
            .cfa(
                address,
                registers,
                &mut PtraceMemory {
                    ptrace: &self.ptrace,
                    pid,
                },
            )
            .map_err(|termination| variable_cfa_error(&termination))
    }

    /// Locates a frame other than the innermost one as the backtrace
    /// describes it.
    fn outer_frame_location(
        &self,
        inferior: &Inferior,
        pid: Pid,
        frame: StackFrameId,
    ) -> Result<ExecutionLocation> {
        let resolved = self.resolve_frame(inferior, pid, frame)?;
        let selected = resolved.frame.ok_or(Error::AmbiguousInlineFrame)?;
        let (module, address) = resolved.code.ok_or(Error::AddressOutsideModule)?;
        let module = self
            .modules
            .get(&module)
            .ok_or(Error::ModuleNotLoaded(module))?;
        let mut location = module.image.locate(address);
        location.function = selected.function;
        location.source = selected.source;
        // A caller is located just before its return address, but its
        // symbol offset describes the frame's own instruction.
        let lookup = module.loaded.virtual_address(address)?;
        if let Some(symbol) = &mut location.symbol {
            symbol.offset += selected.instruction.get() - lookup.get();
        }

        Ok(ExecutionLocation {
            module: module.loaded.id,
            address: selected.instruction,
            image: location,
        })
    }

    /// The main executable's unwind context, used by stepping plans that are
    /// deliberately limited to code described by the main image.
    pub(super) fn main_unwind_module<'a>(&'a self, inferior: &Inferior) -> UnwindModule<'a> {
        UnwindModule {
            loaded: inferior.loaded_module,
            image: &self.module_image,
            unwind: self.unwind_info.as_ref(),
        }
    }

    /// Every loaded module's unwind context, beginning with the main image.
    pub(super) fn unwind_modules<'a>(&'a self, inferior: &Inferior) -> Vec<UnwindModule<'a>> {
        std::iter::once(self.main_unwind_module(inferior))
            .chain(
                self.modules
                    .values()
                    .filter(|module| module.loaded.id != inferior.loaded_module.id)
                    .map(|module| UnwindModule {
                        loaded: module.loaded,
                        image: &module.image,
                        unwind: module.unwind.as_ref(),
                    }),
            )
            .collect()
    }
}

impl<P: InspectionOps> Controller<P> {
    pub(super) fn select_frame(
        &mut self,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
    ) -> Result<StackFrame> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_stopped_thread(inferior, pid)?;
        validate_image_current(inferior)?;
        let selected = self
            .resolve_frame(inferior, pid, frame)?
            .frame
            .ok_or(Error::AmbiguousInlineFrame)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior
            .public_stop
            .as_mut()
            .expect("public stop was validated")
            .selected_frames
            .insert(pid, frame);
        self.bump_revision();
        Ok(selected)
    }

    pub(super) fn select_thread(&mut self, stop_id: StopId, pid: Pid) -> Result<()> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_stopped_thread(inferior, pid)?;
        let presentation = self.presentation_for_stopped_thread(pid)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior
            .public_stop
            .as_mut()
            .expect("public stop was validated")
            .presentations
            .insert(pid, presentation);
        inferior.selected_thread = Some(pid);
        self.bump_revision();
        Ok(())
    }
}

pub(super) fn frame_lookup_address(level: u32, context: &FrameContext) -> Option<VirtualAddress> {
    if level == 0 || context.signal_frame {
        Some(context.instruction)
    } else {
        context
            .instruction
            .get()
            .checked_sub(1)
            .map(VirtualAddress::new)
    }
}

pub(super) fn make_presentation(
    instruction: VirtualAddress,
    inline_chain: &[CodeInstanceId],
    visible: usize,
) -> Result<FramePresentation> {
    let hidden = inline_chain
        .len()
        .checked_sub(visible)
        .ok_or(Error::LocationUnavailable)?;
    let hidden_inline_frames = u32::try_from(hidden).map_err(|_| Error::LocationUnavailable)?;
    let frame = visible
        .checked_sub(1)
        .map_or(PresentedFrame::Physical, |index| {
            PresentedFrame::Inline(inline_chain[index])
        });

    Ok(FramePresentation {
        instruction,
        frame,
        hidden_inline_frames,
    })
}

pub(super) fn default_inline_visible_count(
    module_image: &ModuleImage,
    inline_chain: &[CodeInstanceId],
    image_address: ImageAddress,
    reveal_new_inline: bool,
) -> usize {
    inline_chain
        .iter()
        .position(|instance| {
            module_image
                .code_instance(*instance)
                .is_some_and(|instance| {
                    instance
                        .ranges
                        .iter()
                        .any(|range| range.start == image_address)
                })
        })
        .map_or(inline_chain.len(), |index| {
            // `position` is a zero-based frame index; presentation uses a
            // count. Source `step` reveals the newly entered frame, while
            // `next`, `finish`, and instruction stops remain in its parent.
            index + usize::from(reveal_new_inline)
        })
}

pub(super) fn presentation_visible_count(
    location: &ImageLocation,
    presentation: &FramePresentation,
) -> Result<usize> {
    if matches!(presentation.frame, PresentedFrame::Ambiguous(_)) {
        return Err(Error::AmbiguousInlineFrame);
    }
    let InlineFrameLookup::Unique(chain) = &location.inline_frames else {
        return match presentation.frame {
            PresentedFrame::Physical => Ok(0),
            PresentedFrame::Inline(_) | PresentedFrame::Ambiguous(_) => {
                Err(Error::LocationUnavailable)
            }
        };
    };
    let visible = match presentation.frame {
        PresentedFrame::Physical => 0,
        PresentedFrame::Inline(selected) => chain
            .instances
            .iter()
            .position(|instance| *instance == selected)
            .map(|index| index + 1)
            .ok_or(Error::LocationUnavailable)?,
        PresentedFrame::Ambiguous(_) => unreachable!("rejected above"),
    };
    let hidden =
        u32::try_from(chain.instances.len() - visible).map_err(|_| Error::LocationUnavailable)?;
    if hidden != presentation.hidden_inline_frames {
        return Err(Error::LocationUnavailable);
    }

    Ok(visible)
}

pub(super) fn apply_presentation(
    module_image: &ModuleImage,
    location: &mut ImageLocation,
    presentation: &FramePresentation,
) -> Result<()> {
    let visible = presentation_visible_count(location, presentation)?;
    let InlineFrameLookup::Unique(chain) = &location.inline_frames else {
        return Ok(());
    };
    let selected_instance = visible
        .checked_sub(1)
        .and_then(|index| chain.instances.get(index).copied())
        .or(location.physical_instance);
    location.function = selected_instance
        .and_then(|instance| module_image.code_instance(instance))
        .and_then(|instance| module_image.function(instance.function))
        .cloned();
    location.source = if visible < chain.instances.len() {
        chain
            .instances
            .get(visible)
            .and_then(|instance| module_image.code_instance(*instance))
            .and_then(|instance| match &instance.kind {
                CodeInstanceKind::Inline { call_site } => call_site.clone(),
                CodeInstanceKind::OutOfLine => None,
            })
    } else {
        location.source.clone()
    };

    Ok(())
}

pub(super) fn selected_code_instance(
    location: &ImageLocation,
    presentation: &FramePresentation,
) -> Result<Option<CodeInstanceId>> {
    presentation_visible_count(location, presentation)?;

    Ok(match presentation.frame {
        PresentedFrame::Physical => location.physical_instance,
        PresentedFrame::Inline(instance) => Some(instance),
        PresentedFrame::Ambiguous(_) => return Err(Error::AmbiguousInlineFrame),
    })
}

pub(super) fn source_for_code_instance(
    module_image: &ModuleImage,
    location: &ImageLocation,
    selected: CodeInstanceId,
) -> Option<SourceLocation> {
    let InlineFrameLookup::Unique(chain) = &location.inline_frames else {
        return (location.physical_instance == Some(selected))
            .then(|| location.source.clone())
            .flatten();
    };
    let visible = if location.physical_instance == Some(selected) {
        0
    } else {
        chain
            .instances
            .iter()
            .position(|instance| *instance == selected)?
            + 1
    };

    if visible < chain.instances.len() {
        chain
            .instances
            .get(visible)
            .and_then(|instance| module_image.code_instance(*instance))
            .and_then(|instance| match &instance.kind {
                CodeInstanceKind::Inline { call_site } => call_site.clone(),
                CodeInstanceKind::OutOfLine => None,
            })
    } else {
        location.source.clone()
    }
}

pub(super) fn code_instance_is_active(location: &ImageLocation, selected: CodeInstanceId) -> bool {
    if location.physical_instance == Some(selected) {
        return true;
    }
    match &location.inline_frames {
        InlineFrameLookup::None => false,
        InlineFrameLookup::Unique(chain) => chain.instances.contains(&selected),
        InlineFrameLookup::Ambiguous(chains) => chains
            .iter()
            .any(|chain| chain.instances.contains(&selected)),
    }
}

pub(super) fn source_step_destination(
    module_image: &ModuleImage,
    location: &ImageLocation,
    kind: StepKind,
) -> bool {
    location.source.is_some()
        && (kind == StepKind::Out
            || module_image
                .line_entry_containing(location.address)
                .is_some_and(|entry| entry.statement))
}

pub(super) fn source_line_changed(
    start: Option<&SourceLocation>,
    current: Option<&SourceLocation>,
) -> bool {
    current.is_some_and(|current| {
        start.is_none_or(|start| start.file != current.file || start.line != current.line)
    })
}

pub(super) fn expand_inline_backtrace(
    physical: Backtrace,
    modules: &[UnwindModule<'_>],
    presentation: &FramePresentation,
) -> Result<Backtrace> {
    let mut frames = Vec::new();

    for physical_frame in physical.frames.iter() {
        let context = FrameContext {
            instruction: physical_frame.instruction,
            cfa: None,
            signal_frame: physical_frame.kind == FrameKind::Signal,
        };
        let lookup = frame_lookup_address(physical_frame.level, &context);
        let located = lookup.and_then(|address| unwind_module_for(modules, address));
        let (Some(lookup), Some((frame_module, image_address))) = (lookup, located) else {
            let level = u32::try_from(frames.len()).expect("frame count fits in u32");
            frames.push(StackFrame::new(
                level,
                physical_frame.kind,
                None,
                physical_frame.instruction,
            ));
            continue;
        };
        let module_image = frame_module.image;
        let location = module_image.locate(image_address);
        let module = Some(frame_module.loaded.id);
        let physical_source = if let InlineFrameLookup::Unique(chain) = &location.inline_frames {
            // The stop presentation describes the main image only; innermost
            // frames in other modules show their complete inline chain.
            let visible =
                if physical_frame.level == 0 && frame_module.loaded.id == modules[0].loaded.id {
                    presentation_visible_count(&location, presentation)?
                } else {
                    chain.instances.len()
                };
            let mut source = if visible < chain.instances.len() {
                chain
                    .instances
                    .get(visible)
                    .and_then(|instance| module_image.code_instance(*instance))
                    .and_then(|instance| match &instance.kind {
                        CodeInstanceKind::Inline { call_site } => call_site.clone(),
                        CodeInstanceKind::OutOfLine => None,
                    })
            } else {
                location.source.clone()
            };

            for &instance_id in chain.instances[..visible].iter().rev() {
                let instance = module_image
                    .code_instance(instance_id)
                    .expect("inline chain references a known instance");
                let function = module_image.function(instance.function).cloned();
                let level = u32::try_from(frames.len()).expect("frame count fits in u32");

                frames.push(StackFrame::from_parts(
                    level,
                    FrameKind::Inline,
                    module,
                    physical_frame.instruction,
                    FrameMetadata {
                        code_instance: Some(instance.id),
                        function,
                        source,
                        symbol: None,
                    },
                ));
                source = match &instance.kind {
                    CodeInstanceKind::Inline { call_site } => call_site.clone(),
                    CodeInstanceKind::OutOfLine => None,
                };
            }
            source
        } else {
            location.source.clone()
        };

        let physical_instance = location
            .physical_instance
            .and_then(|instance| module_image.code_instance(instance));
        let function = physical_instance
            .and_then(|instance| module_image.function(instance.function))
            .cloned();
        let level = u32::try_from(frames.len()).expect("frame count fits in u32");

        frames.push(StackFrame::from_parts(
            level,
            physical_frame.kind,
            module,
            physical_frame.instruction,
            FrameMetadata {
                code_instance: physical_instance.map(|instance| instance.id),
                function,
                source: physical_source,
                // A caller is looked up just before its return address, but
                // its offset describes the frame's own instruction.
                symbol: location.symbol.clone().map(|mut symbol| {
                    symbol.offset += physical_frame.instruction.get() - lookup.get();
                    symbol
                }),
            },
        ));
    }

    Ok(Backtrace {
        thread: physical.thread,
        frames: frames.into(),
        termination: physical.termination,
    })
}

/// One loaded module's address mapping, metadata, and call-frame information.
#[derive(Clone, Copy)]
pub(super) struct UnwindModule<'a> {
    pub(super) loaded: LoadedModule,
    pub(super) image: &'a ModuleImage,
    pub(super) unwind: &'a dyn UnwindInfo,
}

/// One physical activation and the registers it held: the thread's own
/// for the innermost activation, otherwise those the unwinder reconstructed.
pub(super) struct PhysicalFrame {
    pub(super) context: FrameContext,
    pub(super) registers: RegisterFile,
}

/// A stopped thread's physical activations, innermost first.
pub(super) struct PhysicalStack {
    pub(super) native: libc::user_regs_struct,
    pub(super) frames: Vec<PhysicalFrame>,
    pub(super) termination: UnwindTermination,
}

impl PhysicalStack {
    /// Describes the activations as physical backtrace frames, whose module
    /// and symbol metadata the inline expansion resolves.
    fn backtrace(&self, pid: Pid) -> Backtrace {
        Backtrace {
            thread: debug_thread_id(pid),
            frames: self
                .frames
                .iter()
                .enumerate()
                .map(|(level, frame)| {
                    StackFrame::new(
                        u32::try_from(level).expect("frame count fits u32"),
                        if frame.context.signal_frame {
                            FrameKind::Signal
                        } else {
                            FrameKind::Physical
                        },
                        None,
                        frame.context.instruction,
                    )
                })
                .collect(),
            termination: self.termination.clone(),
        }
    }
}

/// The registers a logical frame's values are read from.
pub(super) enum FrameRegisters {
    /// The thread's own registers, which every logical frame of the
    /// innermost activation shares.
    Thread(libc::user_regs_struct),
    /// The registers the unwinder reconstructed for a caller's activation.
    /// Registers a callee could overwrite without saving are absent.
    Caller(RegisterFile),
}

/// The source scope whose variables a frame shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FrameScope {
    /// No single function scope applies: the code has no debug information,
    /// or an innermost frame has no single compatible inline chain.
    Unavailable,
    /// The physical function's own scope.
    Function,
    /// One inline instance's scope.
    Inline(CodeInstanceId),
}

/// One logical frame of a stopped thread and the state that evaluates its
/// variables.
pub(super) struct ResolvedFrame {
    pub(super) id: StackFrameId,
    /// The logical frame whose source scope the frame's variables follow.
    pub(super) presented: PresentedFrame,
    /// The frame as the backtrace describes it; absent for an innermost
    /// frame whose inline presentation is ambiguous.
    pub(super) frame: Option<StackFrame>,
    /// The module describing the frame's code, and the frame's address in
    /// its image: the instruction itself for the innermost activation and
    /// signal frames, otherwise the byte before the return address.
    pub(super) code: Option<(ModuleId, ImageAddress)>,
    pub(super) scope: FrameScope,
    pub(super) registers: FrameRegisters,
    pub(super) cfa: std::result::Result<VirtualAddress, VariableRuntimeError>,
    /// The index of the physical activation containing the frame.
    pub(super) activation: usize,
}

/// Finds the module whose image describes `address`.
///
/// A process address space cannot map two images at one address, so the
/// first describing module is the only one.
pub(super) fn unwind_module_for<'a>(
    modules: &[UnwindModule<'a>],
    address: VirtualAddress,
) -> Option<(UnwindModule<'a>, ImageAddress)> {
    modules.iter().find_map(|module| {
        module
            .loaded
            .image_address(address)
            .ok()
            .filter(|image_address| module.image.contains_address(*image_address))
            .map(|image_address| (*module, image_address))
    })
}

/// Describes a process address by the module whose image covers it.
pub(super) fn describe_address(
    modules: &[UnwindModule<'_>],
    address: VirtualAddress,
) -> AddressDescription {
    AddressDescription {
        address,
        module: unwind_module_for(modules, address).map(|(module, image_address)| ModuleAddress {
            module: module.loaded.id,
            path: module.image.path_arc(),
            image: module.image.describe(image_address),
        }),
    }
}

pub(super) struct DwarfCallerProvider<'a> {
    pub(super) modules: Vec<UnwindModule<'a>>,
    pub(super) registers: RegisterFile,
    pub(super) memory: PtraceMemory<'a>,
    pub(super) first: bool,
}

impl CallerProvider for DwarfCallerProvider<'_> {
    fn caller(&mut self, current: &FrameContext) -> CallerResult {
        let lookup = if self.first || current.signal_frame {
            current.instruction
        } else {
            let Some(address) = current.instruction.get().checked_sub(1) else {
                return CallerResult::Finished(UnwindTermination::Complete);
            };
            VirtualAddress::new(address)
        };
        self.first = false;
        let Some((module, image_address)) = unwind_module_for(&self.modules, lookup) else {
            return CallerResult::Finished(UnwindTermination::ModuleNotFound { address: lookup });
        };
        let step = match module
            .unwind
            .unwind(image_address, &self.registers, &mut self.memory)
        {
            Ok(step) => step,
            Err(mut termination) => {
                if let UnwindTermination::NoUnwindInfo { address } = &mut termination {
                    *address = lookup;
                }
                return CallerResult::Finished(termination);
            }
        };
        let Some(instruction) = step.registers.get(16) else {
            return CallerResult::Finished(UnwindTermination::Complete);
        };
        if instruction == 0 {
            return CallerResult::Finished(UnwindTermination::Complete);
        }
        if step.cfa.get() == current.cfa.map_or(0, VirtualAddress::get)
            && instruction == current.instruction.get()
        {
            return CallerResult::Finished(UnwindTermination::InvalidCaller {
                description: "caller did not make progress".into(),
            });
        }

        self.registers = step.registers;
        CallerResult::Caller(FrameContext {
            instruction: VirtualAddress::new(instruction),
            cfa: Some(step.cfa),
            signal_frame: step.signal_frame,
        })
    }
}
