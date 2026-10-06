//! What the client inspects at a stop for the semantic oracles: backtraces,
//! steps, and the selected frame's variables.

use std::sync::Arc;

use super::{Client, Evaluated, Observation, Purpose, protocol};
use crate::sim::choices::Stream;
use crate::sim::marks::Mark;
use crate::sim::report::Failure;
use crate::{
    Backtrace, Error, Expression, FrameKind, PresentedFrame, ScalarValue, StackFrameId,
    StateSnapshot, StepKind, StopContext, StopId, ThreadState, UnwindTermination, VariableSnapshot,
    VariableState, VariableValue, VariableValueSource, VirtualAddress,
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
        // Half the inspections also evaluate expressions, so that the
        // variables oracle alone judges the rest.
        let evaluations = if self.draw(2) == 0 {
            self.evaluations(&variables, &backtrace).await
        } else {
            Vec::new()
        };
        self.observe(Observation::Variables {
            stop,
            variables,
            backtrace,
            evaluations,
        });
        // Half the inspections of a program with views also present its
        // containers.
        if self.script.views.is_some() && self.draw(2) == 0 {
            self.present(stop).await?;
        }
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

impl Client {
    /// Evaluates each container the program defines, and reads the
    /// elements of each a view presents as a sequence or map in one page
    /// and in pages of a size it draws.
    async fn present(&self, stop: StopId) -> Result<(), Failure> {
        for name in crate::sim::views::CONTAINERS {
            if !self
                .script
                .globals
                .iter()
                .any(|(global, ..)| global == name)
            {
                continue;
            }
            let expression = Expression::parse(name).expect("a container's name parses");
            let value = match self.handle.evaluate(&expression).await {
                Ok(crate::Evaluation::Value { value, .. }) => value,
                Ok(other) => {
                    return Err(protocol(format!("`{name}` evaluated to {other:?}")));
                }
                Err(error) => return Err(protocol(format!("evaluating `{name}` failed: {error}"))),
            };
            let children = match &value.state {
                VariableState::Available {
                    presentation: Some(presentation),
                    ..
                } if matches!(
                    presentation.shape,
                    crate::PresentedShape::Sequence | crate::PresentedShape::Map
                ) =>
                {
                    match &presentation.children {
                        crate::ValueChildren::Available(reference) => Some(Arc::clone(reference)),
                        _ => None,
                    }
                }
                _ => None,
            };
            let (whole, paged) = match children {
                Some(reference) => {
                    let elements = reference.elements().unwrap_or(0);
                    let whole = self.pages(&reference, elements, elements.max(1)).await;
                    let size = 1 + self.draw(3);
                    let paged = self.pages(&reference, elements, size).await;
                    (whole, paged)
                }
                None => (Ok(Vec::new()), Ok(Vec::new())),
            };
            self.note(format!("presented `{name}`"));
            self.observe(Observation::Presented {
                stop,
                name: name.to_owned(),
                value: Box::new(value),
                whole,
                paged,
            });
        }
        Ok(())
    }

    /// The first `elements` children of a presentation, in pages of `size`,
    /// each resuming where the one before it ended, until one comes back
    /// empty, as a page whose budget ran out at once does.
    async fn pages(
        &self,
        reference: &Arc<crate::ValueChildrenReference>,
        elements: u64,
        size: u64,
    ) -> Result<Vec<crate::ValueChildPage>, String> {
        let mut pages = Vec::new();
        let mut offset = 0;
        while offset < elements {
            let limit = u32::try_from(size.min(elements - offset)).expect("small pages");
            let page = self
                .handle
                .value_children(
                    Arc::clone(reference),
                    crate::ValueChildQuery { offset, limit },
                )
                .await
                .map_err(|error| error.to_string())?;
            if page.children.is_empty() {
                break;
            }
            offset += page.children.len() as u64;
            pages.push(page);
        }
        Ok(pages)
    }

    /// Evaluates, in the selected frame, the condition of the marker on its
    /// line, the negation, and what it expects; a few of its variables by
    /// name, and those in memory by address; and a few sums, differences,
    /// products, and casts of its integer variables, and an ill-typed
    /// expression over one.
    async fn evaluations(
        &self,
        variables: &VariableSnapshot,
        backtrace: &Backtrace,
    ) -> Vec<Evaluated> {
        let mut asked = Vec::new();
        let line = backtrace
            .frames
            .iter()
            .find(|frame| frame.id == variables.stack_frame)
            .and_then(|frame| frame.source.as_ref())
            .map(|source| source.line.get());
        if let Some(condition) = line.and_then(|line| self.script.markers.get(&line)) {
            asked.push((Purpose::Marker { negated: false }, condition.clone()));
            asked.push((Purpose::Marker { negated: true }, format!("!({condition})")));
        }
        if let Some(expected) = line.and_then(|line| self.script.expectations.get(&line)) {
            asked.push((Purpose::Expected, expected.clone()));
        }
        // Only names shown once, which name one variable unambiguously.
        let unique = variables.variables.iter().filter(|variable| {
            variables
                .variables
                .iter()
                .filter(|other| other.name == variable.name)
                .count()
                == 1
        });
        let integers = unique
            .clone()
            .filter(|variable| integer(&variable.state).is_some())
            .map(|variable| variable.name.to_string())
            .collect::<Vec<_>>();
        let named = unique.filter(|variable| {
            integer(&variable.state).is_some()
                || matches!(
                    variable.state,
                    VariableState::Unavailable(_)
                        | VariableState::Available {
                            value: VariableValue::Address(_),
                            ..
                        }
                )
        });
        let mut by_name = Vec::new();
        for variable in named.take(4) {
            let name = variable.name.to_string();
            by_name.push((Purpose::Name(name.clone()), name.clone()));
            if let VariableState::Available {
                source: VariableValueSource::Memory(_),
                ..
            } = variable.state
            {
                by_name.push((Purpose::Address(name.clone()), format!("&{name}")));
                by_name.push((Purpose::Name(name.clone()), format!("*&{name}")));
            }
        }
        let pick = |count: usize| usize::try_from(self.draw(count as u64)).expect("small");
        let computed = if integers.is_empty() {
            Vec::new()
        } else {
            self.computed(&integers)
        };
        // Either kind may come first, so that either may be the first to
        // notice a value that changed between reads.
        if pick(2) == 0 {
            asked.extend(by_name.into_iter().chain(computed));
        } else {
            asked.extend(computed.into_iter().chain(by_name));
        }
        let mut evaluations = Vec::new();
        for (purpose, text) in asked {
            let result = match Expression::parse(&text) {
                Ok(expression) => self
                    .handle
                    .evaluate(&expression)
                    .await
                    .map_err(|error| error.to_string()),
                Err(error) => Err(format!("parsing failed: {error}")),
            };
            self.note(format!("evaluated `{text}`"));
            evaluations.push(Evaluated {
                purpose,
                text,
                result,
            });
        }
        evaluations
    }

    /// Draws expressions computed from `integers`, the names of integer
    /// variables: two sums, differences, or products, a cast to a narrower
    /// type, and an ill-typed expression.
    fn computed(&self, integers: &[String]) -> Vec<(Purpose, String)> {
        let pick = |count: usize| usize::try_from(self.draw(count as u64)).expect("small");
        let mut computed = Vec::new();
        for _ in 0..2 {
            let left = integers[pick(integers.len())].clone();
            let right = integers[pick(integers.len())].clone();
            let operator = ['+', '-', '*'][pick(3)];
            let text = format!("{left} {operator} {right}");
            computed.push((
                Purpose::Arithmetic {
                    left,
                    operator,
                    right,
                },
                text,
            ));
        }
        let name = integers[pick(integers.len())].clone();
        let bits = [8, 16, 32][pick(3)];
        let signed = pick(2) == 0;
        let target = format!("{}{bits}", if signed { 'i' } else { 'u' });
        let text = if pick(2) == 0 {
            format!("({target}){name}")
        } else {
            format!("{name} as {target}")
        };
        computed.push((Purpose::Cast { name, bits, signed }, text));
        let name = &integers[pick(integers.len())];
        let text = match pick(3) {
            0 => format!("{name}.no_such_member"),
            1 => format!("{name}[0]"),
            _ => format!("*{name}"),
        };
        computed.push((Purpose::IllTyped, text));
        computed
    }
}

/// An integer variable's value.
fn integer(state: &VariableState) -> Option<i128> {
    match state {
        VariableState::Available {
            value: VariableValue::Scalar(ScalarValue::Signed(value)),
            ..
        } => Some(*value),
        VariableState::Available {
            value: VariableValue::Scalar(ScalarValue::Unsigned(value)),
            ..
        } => i128::try_from(*value).ok(),
        _ => None,
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
