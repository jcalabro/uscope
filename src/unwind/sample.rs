//! Unwinds a stack the kernel sampled, from what a sample carries: the
//! thread's registers, a copy of the top of its stack, and the return
//! addresses the kernel found walking the frame-pointer chain.
//!
//! The copy is unwound by call-frame information first, which is exact
//! wherever it reaches, including leaf functions that keep no frame
//! pointer, which the kernel's walk skips or misreads. Where the copy ends,
//! the stack continues with the kernel's chain from the deepest frame
//! that provably keeps its frame pointer, and every frame after it must
//! provably keep its own for its caller to be believed. A stack that
//! cannot be followed further ends with the reason, never with a guess.

use foldhash::HashMap;

use super::{
    FramePointerRule, MemoryReader, RBP, RETURN_ADDRESS, RSP, RegisterFile, UnwindModule,
    unwind_module_for,
};
use crate::capture::records::Truncation;
use crate::{CodeRole, ModuleImageId, UnwindTermination, VirtualAddress};

/// One sampled thread's state.
#[derive(Debug, Clone, Copy)]
pub struct SampledState<'a> {
    /// The user registers by DWARF number.
    pub registers: &'a [(u16, u64)],
    /// The stack's bytes from the stack pointer up.
    pub stack: &'a [u8],
    /// The kernel's frame-pointer walk: the instruction pointer, then
    /// return addresses, innermost first.
    pub chain: &'a [u64],
}

/// One frame of an unwound sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampledFrame {
    /// The address whose code the frame runs: the instruction pointer of
    /// the innermost frame and of a frame a signal interrupted, and
    /// otherwise the byte before its return address, inside the call.
    pub address: VirtualAddress,
    /// The module covering the address, by index in the modules.
    pub module: Option<usize>,
    pub role: CodeRole,
}

/// How an unwound stack ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackEnd {
    /// At its outermost frame.
    Complete,
    /// Short of it, for this reason, with the module at fault if any.
    Truncated(Truncation, Option<usize>),
}

/// Remembers each instruction's [`FramePointerRule`], which every sample
/// through it asks again.
#[derive(Debug, Default)]
pub struct FrameRules {
    rules: HashMap<(ModuleImageId, u64), FramePointerRule>,
}

impl FrameRules {
    fn rule(&mut self, module: &UnwindModule<'_>, address: VirtualAddress) -> FramePointerRule {
        let Ok(image_address) = module.loaded.image_address(address) else {
            return FramePointerRule::NoUnwindInfo;
        };
        *self
            .rules
            .entry((module.loaded.image, image_address.get()))
            .or_insert_with(|| module.unwind.frame_pointer_rule(image_address))
    }
}

/// The stack bytes a sample copied, at the stack pointer's address.
struct StackCopy<'a> {
    start: u64,
    bytes: &'a [u8],
}

impl MemoryReader for StackCopy<'_> {
    fn read_u64(&mut self, address: VirtualAddress) -> Option<u64> {
        let offset = usize::try_from(address.get().checked_sub(self.start)?).ok()?;
        let word = self.bytes.get(offset..offset.checked_add(8)?)?;
        Some(u64::from_le_bytes(word.try_into().ok()?))
    }
}

/// A frame call-frame information reached, with its own rbp.
struct Unwound {
    frame: SampledFrame,
    frame_pointer: Option<u64>,
}

/// Unwinds one sample through `modules`, innermost frame first, collecting
/// at most `limit` frames.
pub fn unwind_sample(
    modules: &[UnwindModule<'_>],
    rules: &mut FrameRules,
    sample: SampledState<'_>,
    limit: usize,
) -> (Vec<SampledFrame>, StackEnd) {
    let mut registers = RegisterFile::new(sample.registers.iter().copied());
    let Some(instruction) = registers
        .get(RETURN_ADDRESS)
        .or_else(|| sample.chain.first().copied())
    else {
        return (Vec::new(), StackEnd::Truncated(Truncation::NoStack, None));
    };
    let mut memory = StackCopy {
        start: registers.get(RSP).unwrap_or(0),
        bytes: sample.stack,
    };

    // Call-frame information over the copy.
    let mut unwound: Vec<Unwound> = Vec::new();
    let mut address = VirtualAddress::new(instruction);
    let mut previous_cfa = None;
    let cfi_end = loop {
        let found = unwind_module_for(modules, address);
        let module = found.map(|(module, _)| module);
        let index = module.and_then(|found| {
            modules
                .iter()
                .position(|module| module.loaded.id == found.loaded.id)
        });
        let role = found.map_or(CodeRole::Ordinary, |(module, image_address)| {
            module.image.code_role(image_address)
        });
        unwound.push(Unwound {
            frame: SampledFrame {
                address,
                module: index,
                role,
            },
            frame_pointer: registers.get(RBP),
        });
        if unwound.len() >= limit {
            return (
                frames(unwound),
                StackEnd::Truncated(Truncation::DepthLimit, None),
            );
        }
        if role == CodeRole::Outermost {
            return (frames(unwound), StackEnd::Complete);
        }
        let Some((module, image_address)) = found else {
            break StackEnd::Truncated(Truncation::UnmappedAddress, None);
        };
        let step = match module.unwind.unwind(image_address, &registers, &mut memory) {
            Ok(step) => step,
            Err(UnwindTermination::Complete) => return (frames(unwound), StackEnd::Complete),
            Err(UnwindTermination::NoUnwindInfo { .. }) => {
                break StackEnd::Truncated(Truncation::NoUnwindInfo, index);
            }
            Err(UnwindTermination::MemoryReadFailed { .. }) => {
                break StackEnd::Truncated(Truncation::StackCopyExhausted, None);
            }
            Err(_) => break StackEnd::Truncated(Truncation::InvalidCaller, index),
        };
        let caller = match step.registers.get(RETURN_ADDRESS) {
            None | Some(0) => return (frames(unwound), StackEnd::Complete),
            Some(caller) => caller,
        };
        // A caller in the same frame as its callee would repeat forever.
        if previous_cfa == Some(step.cfa) {
            break StackEnd::Truncated(Truncation::InvalidCaller, index);
        }
        previous_cfa = Some(step.cfa);
        address = if step.signal_frame {
            VirtualAddress::new(caller)
        } else {
            VirtualAddress::new(caller.wrapping_sub(1))
        };
        registers = step.registers;
    };

    // The kernel's chain from the deepest frame that keeps the chain.
    let Some((kept, link)) = splice(modules, rules, &unwound, sample, &mut memory) else {
        return (frames(unwound), cfi_end);
    };
    unwound.truncate(kept + 1);
    let mut frames = frames(unwound);
    let end = follow_chain(modules, rules, &mut frames, &sample.chain[link..], limit);
    (frames, end)
}

fn frames(unwound: Vec<Unwound>) -> Vec<SampledFrame> {
    unwound.into_iter().map(|unwound| unwound.frame).collect()
}

/// The deepest frame call-frame information reached whose rbp is a link
/// of the kernel's walk, with the index of the chain's entry for its
/// caller, if the frame provably keeps the chain there.
///
/// The walk over the copy repeats the kernel's: from rbp, each link holds
/// the next link, and the word above it a return address, which must be
/// the chain's.
fn splice(
    modules: &[UnwindModule<'_>],
    rules: &mut FrameRules,
    unwound: &[Unwound],
    sample: SampledState<'_>,
    memory: &mut StackCopy<'_>,
) -> Option<(usize, usize)> {
    let mut links = Vec::new();
    let mut link = sample
        .registers
        .iter()
        .find_map(|(register, value)| (*register == RBP).then_some(*value));
    while let Some(current) = link {
        links.push(current);
        if links.len() >= sample.chain.len() {
            break;
        }
        match memory.read_u64(VirtualAddress::new(current.wrapping_add(8))) {
            // A link whose return address is in the copy must agree with
            // the kernel's, or the two walks part.
            Some(return_address) if return_address != sample.chain[links.len()] => break,
            _ => {}
        }
        link = memory.read_u64(VirtualAddress::new(current));
    }
    for (index, frame) in unwound.iter().enumerate().rev() {
        let Some(frame_pointer) = frame.frame_pointer else {
            continue;
        };
        let Some(position) = links.iter().position(|link| *link == frame_pointer) else {
            continue;
        };
        let caller = position + 1;
        if caller >= sample.chain.len() {
            continue;
        }
        let Some(module) = frame.frame.module.map(|module| &modules[module]) else {
            continue;
        };
        if rules.rule(module, frame.frame.address) != FramePointerRule::Kept {
            continue;
        }
        // Where call-frame information found the caller too, both must
        // name the same one.
        if let Some(next) = unwound.get(index + 1)
            && next.frame.address.get().wrapping_add(1) != sample.chain[caller]
        {
            continue;
        }
        return Some((index, caller));
    }
    None
}

/// Appends the callers `chain` lists, beginning with the caller of the
/// last of `frames`, while each frame provably keeps the chain.
fn follow_chain(
    modules: &[UnwindModule<'_>],
    rules: &mut FrameRules,
    frames: &mut Vec<SampledFrame>,
    chain: &[u64],
    limit: usize,
) -> StackEnd {
    for (position, &return_address) in chain.iter().enumerate() {
        let address = VirtualAddress::new(return_address.wrapping_sub(1));
        let Some((module, image_address)) = unwind_module_for(modules, address) else {
            return StackEnd::Truncated(Truncation::UnmappedAddress, None);
        };
        let index = modules
            .iter()
            .position(|candidate| candidate.loaded.id == module.loaded.id);
        let role = module.image.code_role(image_address);
        frames.push(SampledFrame {
            address,
            module: index,
            role,
        });
        if role == CodeRole::Outermost {
            return StackEnd::Complete;
        }
        if frames.len() >= limit {
            return StackEnd::Truncated(Truncation::DepthLimit, None);
        }
        match rules.rule(&module, address) {
            FramePointerRule::Kept if position + 1 < chain.len() => {}
            FramePointerRule::Kept => return StackEnd::Truncated(Truncation::ChainEnded, index),
            FramePointerRule::Outermost => return StackEnd::Complete,
            FramePointerRule::NotKept => {
                return StackEnd::Truncated(Truncation::FramePointerNotKept, index);
            }
            FramePointerRule::NoUnwindInfo => {
                return StackEnd::Truncated(Truncation::NoUnwindInfo, index);
            }
        }
    }
    StackEnd::Truncated(Truncation::ChainEnded, None)
}
