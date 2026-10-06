//! The generic unwind loop: it iterates the caller contexts a
//! [`CallerProvider`] reconstructs.

use std::collections::{BTreeMap, HashSet};

use crate::{UnwindTermination, VirtualAddress};

pub const DEFAULT_MAX_FRAMES: usize = 256;

/// What the unwinder knows about one frame.
#[derive(Debug, Clone)]
pub struct FrameContext {
    pub instruction: VirtualAddress,
    pub cfa: Option<VirtualAddress>,
    pub signal_frame: bool,
}

/// Register values by DWARF number. An absent register is unknown.
#[derive(Debug, Clone)]
pub struct RegisterFile {
    values: BTreeMap<u16, u64>,
}

impl RegisterFile {
    pub fn new(values: impl IntoIterator<Item = (u16, u64)>) -> Self {
        Self {
            values: values.into_iter().collect(),
        }
    }

    pub fn get(&self, register: u16) -> Option<u64> {
        self.values.get(&register).copied()
    }

    pub fn set(&mut self, register: u16, value: u64) {
        self.values.insert(register, value);
    }

    pub fn remove(&mut self, register: u16) {
        self.values.remove(&register);
    }
}

/// Reads target memory for unwind rules.
pub trait MemoryReader {
    /// Reads one native word, or `None` when the address is unreadable.
    fn read_u64(&mut self, address: VirtualAddress) -> Option<u64>;
}

pub struct UnwindStep {
    pub registers: RegisterFile,
    pub cfa: VirtualAddress,
    pub signal_frame: bool,
}

pub enum CallerResult {
    Caller(FrameContext),
    Finished(UnwindTermination),
}

pub trait CallerProvider {
    fn caller(&mut self, current: &FrameContext) -> CallerResult;
}

/// Unwinds from `initial` until the provider finishes, a frame repeats, or
/// `max_frames` frames are collected. `make_frame` sees each frame with the
/// provider that reconstructed it, so it can capture the frame's registers.
pub fn collect_frames<P: CallerProvider, F>(
    initial: FrameContext,
    provider: &mut P,
    mut make_frame: impl FnMut(u32, &FrameContext, &P) -> F,
    max_frames: usize,
) -> (Vec<F>, UnwindTermination) {
    let mut frames = Vec::new();
    let mut visited = HashSet::new();
    let mut current = initial;

    let termination = loop {
        // A repeated frame state would repeat every frame after it.
        if !visited.insert((current.cfa, current.instruction)) {
            break UnwindTermination::CycleDetected;
        }
        let level = u32::try_from(frames.len()).expect("frame limit fits in u32");
        frames.push(make_frame(level, &current, provider));
        if frames.len() >= max_frames {
            break UnwindTermination::DepthLimit;
        }
        match provider.caller(&current) {
            CallerResult::Caller(caller) => current = caller,
            CallerResult::Finished(termination) => break termination,
        }
    };
    (frames, termination)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns its callers last first, then finishes.
    struct SequenceProvider {
        callers: Vec<FrameContext>,
    }

    impl CallerProvider for SequenceProvider {
        fn caller(&mut self, _current: &FrameContext) -> CallerResult {
            self.callers.pop().map_or_else(
                || CallerResult::Finished(UnwindTermination::Complete),
                CallerResult::Caller,
            )
        }
    }

    fn context(instruction: u64, cfa: u64) -> FrameContext {
        FrameContext {
            instruction: VirtualAddress::new(instruction),
            cfa: Some(VirtualAddress::new(cfa)),
            signal_frame: false,
        }
    }

    #[test]
    fn collection_ends_with_the_provider_at_a_repeated_frame_or_at_the_limit() {
        let collect = |initial, callers, max_frames| {
            collect_frames(
                initial,
                &mut SequenceProvider { callers },
                |_, context, _| context.instruction.get(),
                max_frames,
            )
        };
        let callers = || vec![context(1, 10), context(2, 20)];
        assert_eq!(
            collect(context(3, 30), callers(), 16),
            (vec![3, 2, 1], UnwindTermination::Complete)
        );
        assert_eq!(
            collect(context(1, 10), vec![context(1, 10)], 16),
            (vec![1], UnwindTermination::CycleDetected)
        );
        assert_eq!(
            collect(context(3, 30), callers(), 1),
            (vec![3], UnwindTermination::DepthLimit)
        );
    }
}
