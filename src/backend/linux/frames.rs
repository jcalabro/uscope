//! Stack unwinding and the logical frames presented for inline code.

use std::collections::BTreeSet;

use nix::unistd::Pid;

use crate::debug_info::UnwindInfo;
use crate::model::FrameMetadata;
use crate::protocol::{FramePresentation, PresentedFrame, StepKind, StopId, StopReason};
use crate::unwind::{
    CallerProvider, CallerResult, DEFAULT_MAX_FRAMES, FrameContext, RegisterFile, collect_backtrace,
};
use crate::{
    AddressDescription, Backtrace, CodeInstanceId, CodeInstanceKind, Error, ExecutionLocation,
    FrameKind, ImageAddress, ImageLocation, InlineFrameLookup, LoadedModule, ModuleAddress,
    ModuleImage, Result, SourceLocation, StackFrame, UnwindTermination, VirtualAddress,
};

use super::breakpoints::runtime_breakpoint_address;
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
            Some(StopReason::Breakpoint { address }) => {
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

    pub(super) fn stopped_location(&self, stop_id: StopId, pid: Pid) -> Result<ExecutionLocation> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_stopped_thread(inferior, pid)?;
        validate_image_current(inferior)?;
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

        let native = self.ptrace.registers(pid)?;
        let registers = x86_64_registers(&native);
        let initial = FrameContext {
            instruction: VirtualAddress::new(native.rip),
            cfa: None,
            signal_frame: false,
        };
        let modules = self.unwind_modules(inferior);
        let mut provider = DwarfCallerProvider {
            modules: modules.clone(),
            registers,
            memory: PtraceMemory {
                ptrace: &self.ptrace,
                pid,
            },
            first: true,
        };

        let physical = collect_backtrace(
            debug_thread_id(pid),
            initial,
            &mut provider,
            |level, context| {
                // Module and symbol metadata are resolved by the inline
                // expansion, which sees every physical frame.
                StackFrame::new(
                    level,
                    if context.signal_frame {
                        FrameKind::Signal
                    } else {
                        FrameKind::Physical
                    },
                    None,
                    context.instruction,
                )
            },
            DEFAULT_MAX_FRAMES,
        );

        expand_inline_backtrace(physical, &modules, &presentation)
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
