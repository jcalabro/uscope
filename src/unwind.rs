use std::collections::{BTreeMap, HashSet};

use crate::{Backtrace, StackFrame, ThreadId, UnwindTermination, VirtualAddress};

pub const DEFAULT_MAX_FRAMES: usize = 256;

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

pub trait MemoryReader {
    fn read_u64(&mut self, address: VirtualAddress) -> Result<u64, ()>;
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

pub fn collect_backtrace(
    thread: ThreadId,
    initial: FrameContext,
    provider: &mut impl CallerProvider,
    mut make_frame: impl FnMut(u32, &FrameContext) -> StackFrame,
    max_frames: usize,
) -> Backtrace {
    let mut frames = Vec::new();
    let mut visited = HashSet::new();
    let mut current = initial;

    loop {
        let level = u32::try_from(frames.len()).expect("frame limit fits in u32");
        frames.push(make_frame(level, &current));

        if frames.len() >= max_frames {
            return Backtrace {
                thread,
                frames: frames.into(),
                termination: UnwindTermination::DepthLimit,
            };
        }

        if !visited.insert((current.cfa, current.instruction)) {
            return Backtrace {
                thread,
                frames: frames.into(),
                termination: UnwindTermination::CycleDetected,
            };
        }

        match provider.caller(&current) {
            CallerResult::Caller(caller) => current = caller,
            CallerResult::Finished(termination) => {
                return Backtrace {
                    thread,
                    frames: frames.into(),
                    termination,
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FrameKind;

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

    fn frame(level: u32, context: &FrameContext) -> StackFrame {
        StackFrame::new(
            level,
            if context.signal_frame {
                FrameKind::Signal
            } else {
                FrameKind::Physical
            },
            None,
            context.instruction,
            None,
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

        let trace = collect_backtrace(ThreadId::new(7), initial, &mut provider, frame, 16);

        assert_eq!(
            trace
                .frames
                .iter()
                .map(|frame| frame.instruction.get())
                .collect::<Vec<_>>(),
            [3, 2, 1]
        );
        assert_eq!(trace.frames[1].kind, FrameKind::Signal);
        assert_eq!(trace.termination, UnwindTermination::Complete);
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
        let trace = collect_backtrace(ThreadId::new(1), context.clone(), &mut cyclic, frame, 16);
        assert_eq!(trace.termination, UnwindTermination::CycleDetected);

        let mut deep = SequenceProvider {
            callers: vec![context.clone(), context.clone()],
        };
        let trace = collect_backtrace(ThreadId::new(1), context, &mut deep, frame, 1);
        assert_eq!(trace.frames.len(), 1);
        assert_eq!(trace.termination, UnwindTermination::DepthLimit);
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
        let trace = collect_backtrace(ThreadId::new(1), initial, &mut FailingProvider, frame, 16);

        assert_eq!(trace.frames.len(), 1);
        assert_eq!(
            trace.termination,
            UnwindTermination::NoUnwindInfo {
                address: VirtualAddress::new(0x1234)
            }
        );
    }
}
