//! What the client inspects at a stop for the semantic oracles: backtraces,
//! steps, and the selected frame's variables.

use super::{Client, Observation, protocol};
use crate::sim::choices::Stream;
use crate::sim::marks::Mark;
use crate::sim::report::Failure;
use crate::{
    Backtrace, Error, FrameKind, PresentedFrame, StackFrameId, StateSnapshot, StepKind,
    StopContext, StopId, ThreadState, UnwindTermination, VirtualAddress,
};

impl Client {
    /// Takes a backtrace, which a stop whose inline frame is ambiguous
    /// cannot present.
    pub(super) async fn backtrace(&self) -> Result<(), Failure> {
        let snapshot = self
            .handle
            .snapshot()
            .await
            .map_err(|error| protocol(format!("snapshot failed: {error}")))?;
        match self.handle.backtrace().await {
            Ok(backtrace) => {
                self.note(format!(
                    "backtrace: {} frames, {:?}",
                    backtrace.frames.len(),
                    backtrace.termination
                ));
                if let Some(stop) = snapshot.stop_id {
                    self.observe(Observation::Backtrace { stop, backtrace });
                }
            }
            Err(Error::AmbiguousInlineFrame) if presented_ambiguously(&snapshot) => {
                self.note("backtrace from an ambiguous inline frame refused");
            }
            Err(error) => return Err(protocol(format!("backtrace failed: {error}"))),
        }
        Ok(())
    }

    pub(super) async fn step(&self) -> Result<(), Failure> {
        let kinds = [
            StepKind::Instruction,
            StepKind::OverInstruction,
            StepKind::IntoSource,
            StepKind::OverSource,
            StepKind::Out,
        ];
        let kind = *self.choices.borrow_mut().pick(Stream::Client, &kinds);
        let before = self
            .handle
            .snapshot()
            .await
            .map_err(|error| protocol(format!("snapshot failed: {error}")))?;
        // A source step must know which frame it starts in, so one from a
        // stop whose inline frame is ambiguous is refused, not guessed.
        let ambiguous = kind != StepKind::Instruction
            && kind != StepKind::OverInstruction
            && presented_ambiguously(&before);
        // Stepping out of a frame without a caller to return to must be
        // refused, and change nothing.
        let caller = if kind == StepKind::Out && !ambiguous {
            let backtrace = self
                .handle
                .backtrace()
                .await
                .map_err(|error| protocol(format!("backtrace failed: {error}")))?;
            let caller = Caller::of(&backtrace);
            if let Some(stop) = before.stop_id {
                self.observe(Observation::Backtrace { stop, backtrace });
            }
            caller
        } else {
            Caller::Trusted
        };
        self.note(format!("step {kind:?}"));
        if let Some(thread) = before.selected_thread {
            self.observe(Observation::StepBegins {
                thread,
                kind,
                presentation: before.presentation.clone(),
            });
        }
        let result = self.handle.step(kind).await;
        self.observe(Observation::StepEnded(result.as_ref().ok().cloned()));
        if ambiguous {
            if !matches!(result, Err(Error::AmbiguousInlineFrame)) {
                return Err(protocol(format!(
                    "step {kind:?} from an ambiguous inline frame returned {result:?}"
                )));
            }
            let after = self
                .handle
                .snapshot()
                .await
                .map_err(|error| protocol(format!("snapshot failed: {error}")))?;
            if after != before {
                return Err(protocol(format!(
                    "a refused step changed the state from {before:?} to {after:?}"
                )));
            }
            self.note(format!(
                "step {kind:?} from an ambiguous inline frame refused"
            ));
            return Ok(());
        }
        match (result, &caller) {
            (
                Ok(reason),
                Caller::Trusted | Caller::Outermost(UnwindTermination::NoUnwindInfo { .. }),
            ) => {
                self.note(format!("stepped: {reason:?}"));
            }
            (Err(Error::EventStreamLagged(_)), _) => self.mark(Mark::ClientLagged),
            (Err(error), _) if self.refused_unarmed(&error).await? => {}
            (Err(error), Caller::Outermost(_) | Caller::Corrupt(_)) => {
                let after = self
                    .handle
                    .snapshot()
                    .await
                    .map_err(|error| protocol(format!("snapshot failed: {error}")))?;
                if after != before {
                    return Err(protocol(format!(
                        "a refused step out changed the state from {before:?} to {after:?}"
                    )));
                }
                self.note(format!("step out of a frame {caller} refused: {error}"));
                self.mark(Mark::StepOutRefused);
            }
            (Ok(reason), caller) => {
                return Err(protocol(format!(
                    "stepped out of a frame {caller}: {reason:?}"
                )));
            }
            (Err(error), Caller::Trusted) => {
                return Err(protocol(format!("step {kind:?} failed: {error}")));
            }
        }
        Ok(())
    }

    /// Reads the variables of the selected frame, and the backtrace of its
    /// thread, which the variables oracle judges where a marker applies.
    pub(super) async fn inspect(&self, stop: StopId) -> Result<(), Failure> {
        let snapshot = self
            .handle
            .snapshot()
            .await
            .map_err(|error| protocol(format!("snapshot failed: {error}")))?;
        let backtrace = match self.handle.backtrace().await {
            Ok(backtrace) => backtrace,
            Err(Error::AmbiguousInlineFrame) if presented_ambiguously(&snapshot) => {
                self.note("inspecting an ambiguous inline frame refused");
                return Ok(());
            }
            Err(error) => return Err(protocol(format!("backtrace failed: {error}"))),
        };
        // Code no debug information describes has no variables to show.
        let undescribed = backtrace
            .frames
            .iter()
            .find(|frame| Some(frame.id) == snapshot.selected_frame)
            .is_some_and(|frame| frame.function.is_none());
        let variables = match self.handle.variables().await {
            Ok(variables) => variables,
            Err(Error::VariableContextUnsupported) if undescribed => {
                self.note("variables of code without debug information refused");
                return Ok(());
            }
            Err(error) => return Err(protocol(format!("reading variables failed: {error}"))),
        };
        self.note(format!(
            "variables of frame {} in thread {}: {}",
            variables.stack_frame,
            variables.thread,
            variables
                .variables
                .iter()
                .map(|variable| variable.name.as_ref())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        self.observe(Observation::Backtrace {
            stop,
            backtrace: backtrace.clone(),
        });
        self.observe(Observation::Variables {
            stop,
            variables,
            backtrace,
        });
        // Every other stopped thread's stack, without changing which is
        // selected.
        for thread in snapshot.threads.iter() {
            if Some(thread.id) == snapshot.selected_thread
                || !matches!(thread.state, ThreadState::Stopped { .. })
            {
                continue;
            }
            let context = StopContext {
                stop,
                thread: thread.id,
                frame: StackFrameId::new(0),
            };
            match self.handle.at(context).backtrace().await {
                Ok(backtrace) => self.observe(Observation::Backtrace { stop, backtrace }),
                // Only the selected thread's presentation is in the
                // snapshot, so another's inline frame may be ambiguous.
                Err(Error::AmbiguousInlineFrame) => {
                    self.note(format!("thread {}'s inline frame is ambiguous", thread.id));
                }
                Err(error) => {
                    return Err(protocol(format!(
                        "backtrace of thread {} failed: {error}",
                        thread.id
                    )));
                }
            }
        }
        Ok(())
    }
}

/// What a step out of the innermost frame would return to.
#[derive(Debug, Clone)]
enum Caller {
    /// A caller the unwinder found in the program's code, or none needed:
    /// an inline frame returns into its physical frame.
    Trusted,
    /// The unwinder says the frame has no caller.
    Outermost(UnwindTermination),
    /// The caller the stack names lies outside every module, as a corrupt
    /// return address does.
    Corrupt(VirtualAddress),
}

impl Caller {
    fn of(backtrace: &Backtrace) -> Self {
        let Some(frame) = backtrace.frames.first() else {
            return Self::Outermost(backtrace.termination.clone());
        };
        if frame.kind == FrameKind::Inline {
            return Self::Trusted;
        }
        match backtrace.frames.get(1) {
            None => Self::Outermost(backtrace.termination.clone()),
            Some(caller) if caller.module.is_none() => Self::Corrupt(caller.instruction),
            Some(_) => Self::Trusted,
        }
    }
}

impl std::fmt::Display for Caller {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Trusted => formatter.write_str("with a caller"),
            Self::Outermost(termination) => write!(formatter, "without a caller ({termination})"),
            Self::Corrupt(address) => {
                write!(formatter, "whose caller {address} is outside every module")
            }
        }
    }
}

/// Whether a stop presents its selected thread in an inline frame the
/// debug information leaves ambiguous.
fn presented_ambiguously(snapshot: &StateSnapshot) -> bool {
    snapshot
        .presentation
        .as_ref()
        .is_some_and(|presentation| matches!(presentation.frame, PresentedFrame::Ambiguous(_)))
}
