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
    use crate::{FrameKind, StackFrame};

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

    fn frame<P>(level: u32, context: &FrameContext, _provider: &P) -> StackFrame {
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
    }

    #[test]
    fn collection_preserves_frames_and_completion_reason() {
        let initial = FrameContext {
            instruction: VirtualAddress::new(3),
            cfa: Some(VirtualAddress::new(30)),
            signal_frame: false,
        };
        let mut provider = SequenceProvider {
            callers: vec![
                FrameContext {
                    instruction: VirtualAddress::new(1),
                    cfa: Some(VirtualAddress::new(10)),
                    signal_frame: false,
                },
                FrameContext {
                    instruction: VirtualAddress::new(2),
                    cfa: Some(VirtualAddress::new(20)),
                    signal_frame: true,
                },
            ],
        };

        let (frames, termination) = collect_frames(initial, &mut provider, frame, 16);

        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.instruction.get())
                .collect::<Vec<_>>(),
            [3, 2, 1]
        );
        assert_eq!(frames[1].kind, FrameKind::Signal);
        assert_eq!(termination, UnwindTermination::Complete);
    }

    #[test]
    fn collection_detects_cycles_and_depth_limit() {
        let context = FrameContext {
            instruction: VirtualAddress::new(1),
            cfa: Some(VirtualAddress::new(10)),
            signal_frame: false,
        };
        let mut cyclic = SequenceProvider {
            callers: vec![context.clone(), context.clone()],
        };
        let (frames, termination) = collect_frames(context.clone(), &mut cyclic, frame, 16);
        assert_eq!(frames.len(), 1, "the repeated frame is not shown twice");
        assert_eq!(termination, UnwindTermination::CycleDetected);

        let mut deep = SequenceProvider {
            callers: vec![context.clone(), context.clone()],
        };
        let (frames, termination) = collect_frames(context, &mut deep, frame, 1);
        assert_eq!(frames.len(), 1);
        assert_eq!(termination, UnwindTermination::DepthLimit);
    }

    #[test]
    fn collection_returns_a_valid_prefix_when_the_provider_stops() {
        struct FailingProvider;

        impl CallerProvider for FailingProvider {
            fn caller(&mut self, current: &FrameContext) -> CallerResult {
                CallerResult::Finished(UnwindTermination::NoUnwindInfo {
                    address: current.instruction,
                })
            }
        }

        let initial = FrameContext {
            instruction: VirtualAddress::new(0x1234),
            cfa: None,
            signal_frame: false,
        };
        let (frames, termination) = collect_frames(initial, &mut FailingProvider, frame, 16);

        assert_eq!(frames.len(), 1);
        assert_eq!(
            termination,
            UnwindTermination::NoUnwindInfo {
                address: VirtualAddress::new(0x1234)
            }
        );
    }
}
