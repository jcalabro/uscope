//! The semantic oracles: the debugger's answers about where a program is,
//! how a step moved it, and what its variables hold, against what the
//! program really did.
//!
//! Each judges by the kernel's shadow state (calls made and not returned
//! from, instructions retired, positions a stepping thread passed) and by
//! what binutils say about the binary (`facts`), never by uscope's own
//! reading of the debug information.

use std::collections::{BTreeMap, BTreeSet};

use iced_x86::{Decoder, DecoderOptions, Mnemonic};

use super::client::{Evaluated, Purpose};
use super::corpus::Variant;
use super::facts::{self, Facts, Line};
use super::kernel::shadow::{Position, Shadow};
use super::kernel::{Kernel, State, Thread, Tid};
use super::loader::Image;
use super::markers::{Marker, Verdict};
use super::marks::Mark;
use crate::{
    Backtrace, FrameKind, PresentedFrame, ScalarValue, StepKind, UnwindTermination,
    VariableSnapshot, VariableState, VariableValue, VariableValueSource,
};

/// What a backtrace showed of its thread's stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unwound {
    /// Every frame, to the thread's first.
    Whole,
    /// The innermost frames, ending explicitly where unwinding could not
    /// go on, as at a frame without call-frame information.
    Truncated,
    /// The innermost frames, ending at a caller read from a return address
    /// the program overwrote.
    Corrupt,
}

fn tid_of(thread: crate::ThreadId) -> Tid {
    Tid::try_from(thread.get()).expect("a simulated tid fits")
}

/// The stopped thread `thread` names, if it still has registers and its
/// shadow still describes its stack.
fn stopped(kernel: &Kernel, thread: crate::ThreadId) -> Option<&Thread> {
    kernel
        .threads
        .get(&tid_of(thread))
        .filter(|thread| matches!(thread.state, State::Stopped { .. }) && !thread.shadow.lost)
}

/// Backtrace: the debugger's physical frames are the thread's location and
/// then the return addresses of the calls it has not returned from,
/// innermost first. The debugger may stop early only by saying why. It may
/// show a caller read from a return address the program overwrote, since
/// that is what the stack holds, but nothing past it, and not as a whole
/// stack.
pub fn backtrace(kernel: &Kernel, backtrace: &Backtrace) -> Result<Option<Unwound>, String> {
    let Some(thread) = backtrace
        .context
        .as_thread()
        .and_then(|thread| stopped(kernel, thread))
    else {
        return Ok(None);
    };
    let process = kernel
        .process_of(thread.tid)
        .expect("a stopped thread's process exists");
    let calls = &thread.shadow.calls;
    let depth = calls.len() + 1;
    let complete = backtrace.termination == UnwindTermination::Complete;
    let physical = backtrace
        .frames
        .iter()
        .filter(|frame| frame.kind != FrameKind::Inline)
        .collect::<Vec<_>>();
    for (level, frame) in physical.iter().enumerate() {
        let address = frame.instruction.get();
        if level == 0 {
            if address != thread.registers.rip {
                return Err(format!(
                    "thread {}'s innermost frame is at {address:#x}, but the thread is at {:#x}",
                    thread.tid, thread.registers.rip
                ));
            }
            continue;
        }
        let Some(call) = calls.len().checked_sub(level).map(|index| calls[index]) else {
            return Err(format!(
                "thread {}'s frame {level} is at {address:#x}, but the thread is only {depth} \
                 frames deep: {calls:x?}",
                thread.tid
            ));
        };
        let held = process
            .space
            .peek_bytes(call.slot, 8)
            .map(|bytes| u64::from_le_bytes(bytes[..].try_into().expect("eight bytes")));
        if held != Some(call.return_address) {
            let held = held.unwrap_or_default();
            if address != held {
                return Err(format!(
                    "thread {}'s frame {level} is at {address:#x}; its return address \
                     {:#x} at {:#x} was overwritten with {held:#x}",
                    thread.tid, call.return_address, call.slot
                ));
            }
            if level + 1 != physical.len() || complete {
                return Err(format!(
                    "thread {}'s backtrace goes on past frame {level}, whose return address \
                     the program overwrote, ending {}",
                    thread.tid, backtrace.termination
                ));
            }
            return Ok(Some(Unwound::Corrupt));
        }
        if address != call.return_address {
            return Err(format!(
                "thread {}'s frame {level} is at {address:#x}, but its call returns to {:#x}",
                thread.tid, call.return_address
            ));
        }
    }
    match (physical.len() < depth, complete) {
        (true, true) => Err(format!(
            "thread {}'s backtrace says it is complete after {} frames, but the thread is \
             {depth} frames deep",
            thread.tid,
            physical.len()
        )),
        (true, false) => Ok(Some(Unwound::Truncated)),
        (false, false) => Err(format!(
            "thread {}'s backtrace found all {depth} frames, but ends {}",
            thread.tid, backtrace.termination
        )),
        (false, true) => Ok(Some(Unwound::Whole)),
    }
}

/// Where a step began.
#[derive(Debug, Clone)]
pub struct Begun {
    pub tid: Tid,
    pub kind: StepKind,
    pub rip: u64,
    pub shadow: Shadow,
    /// How many instructions the thread had completed.
    pub retired: u64,
    /// Whether the thread stood inside a system call, which returns before
    /// it executes anything else.
    pub inside_call: bool,
    /// The frame the debugger presented, and how many inline frames it
    /// hid below it.
    pub frame: Option<PresentedFrame>,
    pub hidden_inline_frames: u32,
}

impl Begun {
    /// The step `kind` of `thread`, about to be requested.
    #[must_use]
    pub fn new(
        thread: &Thread,
        kind: StepKind,
        presentation: Option<&crate::FramePresentation>,
    ) -> Self {
        Self {
            tid: thread.tid,
            kind,
            rip: thread.registers.rip,
            shadow: thread.shadow.clone(),
            retired: thread.retired,
            inside_call: thread.returning.is_some(),
            frame: presentation.map(|presentation| presentation.frame.clone()),
            hidden_inline_frames: presentation
                .map_or(0, |presentation| presentation.hidden_inline_frames),
        }
    }
}

/// How a completed step was judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Judged {
    /// By rules that need no knowledge of lines.
    Instructions,
    /// A source step in optimized code, which inlines, splits functions,
    /// and turns calls into jumps: by where it stopped and how deep.
    Loosely,
    /// A source step in unoptimized code: also by every position it passed,
    /// none of which may be a place it had to stop.
    Exactly,
}

/// Stepping: a completed step left its thread where its kind says.
///
/// - An instruction step executes one instruction, or, from inside a
///   system call, finishes the call (K-TRAP-1).
/// - Stepping over an instruction steps one, or runs a call until it
///   returns to the same frame.
/// - Stepping out of a physical frame returns to its caller's return
///   address, or, where no line describes that, goes on to the first
///   instruction one does, in the caller or, if the caller returns first,
///   further out; out of an inline frame, it stays in the physical frame
///   or returns from it.
/// - A source step stops in a statement row of another line, or in a
///   caller's statement row, or, stepping in, wherever a new function
///   begins its source. Stepping over never stops in a callee. In
///   unoptimized code, no position the thread passed was the start of a
///   statement row of another line in the same frame or of a line in a
///   caller other than the one it called from.
pub fn step(
    kernel: &Kernel,
    begun: &Begun,
    positions: &[Position],
    variant: &Variant,
) -> Result<Option<Judged>, String> {
    let Some(thread) = kernel
        .threads
        .get(&begun.tid)
        .filter(|thread| matches!(thread.state, State::Stopped { .. }))
    else {
        return Err(format!(
            "the step of thread {} completed, but the thread is not stopped",
            begun.tid
        ));
    };
    if thread.shadow.lost || begun.shadow.lost {
        return Ok(None);
    }
    let end = thread.registers.rip;
    let retired = thread.retired - begun.retired;
    let start = begun.rip;
    let after = &thread.shadow.calls;
    let before = &begun.shadow.calls;
    let one_instruction = || {
        if retired == 1 || begun.inside_call && retired == 0 && end == start {
            Ok(Some(Judged::Instructions))
        } else {
            Err(format!(
                "a {:?} step from {start:#x} executed {retired} instructions, ending at {end:#x}",
                begun.kind
            ))
        }
    };
    match begun.kind {
        StepKind::Instruction => one_instruction(),
        StepKind::OverInstruction => {
            let instruction = original_instruction(&variant.image, start)?;
            if instruction.mnemonic() != Mnemonic::Call {
                return one_instruction();
            }
            if end != instruction.next_ip() || after != before {
                return Err(format!(
                    "stepping over the call at {start:#x} ended at {end:#x}, {} calls deep; \
                     it returns to {:#x}, {} calls deep",
                    after.len(),
                    instruction.next_ip(),
                    before.len()
                ));
            }
            Ok(Some(Judged::Instructions))
        }
        StepKind::Out => {
            let lines = Lines::of(variant);
            let described = |address| lines.line(address).is_some();
            // A source step stops only where a line describes the code, so
            // a return to undescribed code goes on to the first instruction
            // of the caller that a line describes.
            // A caller that returns before reaching described code takes the
            // step on to its own caller.
            let returned = before.split_last().is_some_and(|(call, callers)| {
                (end == call.return_address && after.as_slice() == callers)
                    || callers.starts_with(after)
                        && !described(call.return_address)
                        && described(end)
                        && positions
                            .iter()
                            .filter(|position| {
                                position.depth < before.len()
                                    && activation_at(&begun.shadow, position.depth)
                                        == Some(position.activation)
                            })
                            .all(|position| !described(position.rip))
            });
            let inline = matches!(begun.frame, Some(PresentedFrame::Inline(_)));
            if returned || inline && after == before {
                return Ok(Some(Judged::Instructions));
            }
            Err(format!(
                "stepping out of {} frame at {start:#x}, {} calls deep, ended at {end:#x}, {} \
                 calls deep; it returns to {:#x}",
                if inline { "an inline" } else { "the" },
                before.len(),
                after.len(),
                before.last().map_or(0, |call| call.return_address)
            ))
        }
        StepKind::IntoSource | StepKind::OverSource => {
            source_step(begun, thread, retired, positions, variant).map(Some)
        }
    }
}

/// The instruction the program has at `address`, whatever traps the
/// debugger planted over it.
fn original_instruction(image: &Image, address: u64) -> Result<iced_x86::Instruction, String> {
    let bytes = (0..15)
        .map_while(|offset| image.original_byte(address + offset))
        .collect::<Vec<_>>();
    let mut decoder = Decoder::with_ip(64, &bytes, address, DecoderOptions::NONE);
    let instruction = decoder.decode();
    if decoder.last_error() == iced_x86::DecoderError::None {
        Ok(instruction)
    } else {
        Err(format!(
            "a step began at {address:#x}, which holds no instruction"
        ))
    }
}

/// The activation at `depth` among the calls of a thread whose shadow was
/// `shadow`.
fn activation_at(shadow: &Shadow, depth: usize) -> Option<u64> {
    match depth {
        0 => Some(shadow.base),
        depth => shadow.calls.get(depth - 1).map(|call| call.activation),
    }
}

/// The facts about a variant's lines, at the addresses where it loaded.
struct Lines<'a> {
    facts: &'a Facts,
    bias: u64,
}

impl<'a> Lines<'a> {
    fn of(variant: &'a Variant) -> Self {
        Self {
            facts: &variant.facts,
            bias: variant.image.bias(),
        }
    }

    /// The row describing the instruction at a loaded address.
    fn row(&self, address: u64) -> Option<&'a facts::Range> {
        address
            .checked_sub(self.bias)
            .and_then(|image| self.facts.range(image))
    }

    fn line(&self, address: u64) -> Option<Line> {
        self.row(address).and_then(|range| range.line)
    }

    /// The row a statement begins at `address`, if one does.
    fn statement_at(&self, address: u64) -> Option<&'a facts::Range> {
        self.row(address)
            .filter(|range| range.start + self.bias == address && range.statement)
    }

    /// Whether the compiler marks an epilogue at `address`.
    fn epilogue(&self, address: u64) -> bool {
        address
            .checked_sub(self.bias)
            .is_some_and(|image| self.facts.epilogues.contains(&image))
    }

    fn describe(&self, line: Option<Line>) -> String {
        line.map_or_else(
            || "no line".to_owned(),
            |line| format!("{}:{}", self.facts.file(line), line.line),
        )
    }
}

fn source_step(
    begun: &Begun,
    thread: &Thread,
    retired: u64,
    positions: &[Position],
    variant: &Variant,
) -> Result<Judged, String> {
    let lines = Lines::of(variant);
    let (start, end) = (begun.rip, thread.registers.rip);
    let before = &begun.shadow.calls;
    let after = &thread.shadow.calls;
    let from = lines.line(start);
    let describe = |line| lines.describe(line);
    let entered = after.len() > before.len() && after[..before.len()] == before[..];
    // Begun in code no line describes, stepping over has no line to step
    // over, and steps as stepping in does.
    let kind = if lines.row(start).is_none() {
        StepKind::IntoSource
    } else {
        begun.kind
    };

    // Stepping into an inline frame hidden at the stop moves nothing.
    if kind == StepKind::IntoSource && retired == 0 && end == start {
        if begun.hidden_inline_frames > 0 {
            return Ok(Judged::Loosely);
        }
        return Err(format!(
            "a step in at {start:#x} moved nothing, and no inline frame was hidden there"
        ));
    }
    if kind == StepKind::OverSource
        && !(after.len() <= before.len() && before[..after.len()] == after[..])
    {
        return Err(format!(
            "stepping over {} from {start:#x} stopped at {end:#x} in a callee, {} calls deep \
             from {}",
            describe(from),
            after.len(),
            before.len()
        ));
    }
    // A compiler's epilogue marker is where a source step crosses to the
    // caller, never a place it stops.
    if lines.epilogue(end) {
        return Err(format!(
            "a {kind:?} step from {start:#x} ({}) stopped at {end:#x}, where the compiler \
             marks an epilogue",
            describe(from)
        ));
    }
    let Some(stop) = lines.row(end).filter(|range| range.line.is_some()) else {
        return Err(format!(
            "a {kind:?} step from {start:#x} ({}) stopped at {end:#x}, which no source line \
             describes",
            describe(from)
        ));
    };
    // Stepping in may stop where a function's source begins, which
    // optimized code need not mark as a statement.
    if !(stop.statement || kind == StepKind::IntoSource && entered && lines.facts.optimized) {
        return Err(format!(
            "a {kind:?} step from {start:#x} ({}) stopped at {end:#x} ({}), which is not a \
             statement",
            describe(from),
            describe(stop.line)
        ));
    }
    if lines.facts.optimized {
        return Ok(Judged::Loosely);
    }
    if thread.shadow.activation() == begun.shadow.activation() && stop.line == from {
        return Err(format!(
            "a {kind:?} step from {start:#x} stopped at {end:#x} on the line it began on, {}",
            describe(from)
        ));
    }
    if let Some((position, line)) = missed_stop(begun, positions, &lines) {
        return Err(format!(
            "a {kind:?} step from {start:#x} ({}) ran past {:#x} ({}), {} calls deep, where it \
             had to stop, to {end:#x} ({})",
            describe(from),
            position.rip,
            describe(line),
            position.depth,
            describe(stop.line)
        ));
    }
    Ok(Judged::Exactly)
}

/// The first place a source step's thread passed where the step had to
/// stop: the start of a statement of another line in the frame it began
/// in, or in a caller, of a line other than the one the caller called
/// from. Once the step crossed a frame's epilogue marker, it was bound for
/// that frame's caller, and the rest of the frame was no place to stop.
fn missed_stop(
    begun: &Begun,
    positions: &[Position],
    lines: &Lines<'_>,
) -> Option<(Position, Option<Line>)> {
    let from = lines.line(begun.rip);
    let before = &begun.shadow.calls;
    let mut crossed = BTreeSet::new();
    positions.iter().find_map(|&position| {
        if lines.epilogue(position.rip) {
            crossed.insert(position.activation);
        }
        if crossed.contains(&position.activation) {
            return None;
        }
        let line = lines.statement_at(position.rip)?.line;
        if line.is_none() || line == from {
            return None;
        }
        let must_stop = position.activation == begun.shadow.activation()
            || position.depth < before.len()
                && activation_at(&begun.shadow, position.depth) == Some(position.activation)
                && lines.line(before[position.depth].return_address) != line;
        must_stop.then_some((position, line))
    })
}

/// What the variables oracle found at a stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inspected {
    /// A marker's condition held with the debugger's values.
    Held,
    /// A marker applied, but optimized code left a variable it needs
    /// without a value.
    Unavailable,
}

/// Variables: where a thread stands at the start of a line that carries a
/// marker, and the debugger presents that line in its innermost frame,
/// the variables it shows there satisfy the marker's condition. Variables
/// in unoptimized code always have values.
pub fn variables(
    kernel: &Kernel,
    snapshot: &VariableSnapshot,
    backtrace: &Backtrace,
    variant: &Variant,
    source: &str,
    markers: &[Marker],
) -> Result<Option<Inspected>, String> {
    let Some(marker) = marker_at(kernel, snapshot, backtrace, variant, source, markers) else {
        return Ok(None);
    };
    let mut values = BTreeMap::new();
    for name in marker.condition.variables() {
        let state = snapshot
            .variables
            .iter()
            .rev()
            .find(|variable| &*variable.name == name)
            .map(|variable| &variable.state);
        match state.and_then(integer) {
            Some(value) => {
                values.insert(name.to_owned(), value);
            }
            None if !variant.facts.optimized => {
                return Err(format!(
                    "at line {} of {source}, unoptimized code, variable {name} is {state:?}",
                    marker.line
                ));
            }
            None => {}
        }
    }
    match marker.condition.evaluate(&values) {
        Verdict::Holds => Ok(Some(Inspected::Held)),
        Verdict::Unknown(_) => Ok(Some(Inspected::Unavailable)),
        Verdict::Fails(comparison) => Err(format!(
            "at line {} of {source}, {}: {comparison}",
            marker.line, marker.text
        )),
    }
}

/// Expressions, evaluated in the frame whose variables the client read at
/// the same stop: where the variables oracle applies, a marker's condition
/// evaluates true and its negation false; a variable's name, or its
/// address dereferenced, evaluates to what the variables view shows, from
/// bytes the simulated memory holds where the debugger says it read them;
/// `&x` is where the view says `x` lives; and sums, differences, and
/// products of integer variables are exact, never wrapped to a machine
/// width. Returns the marks the evaluations reached.
pub fn evaluations(
    kernel: &Kernel,
    snapshot: &VariableSnapshot,
    backtrace: &Backtrace,
    variant: &Variant,
    source: &str,
    markers: &[Marker],
    evaluations: &[Evaluated],
) -> Result<Vec<Mark>, String> {
    let marker = marker_at(kernel, snapshot, backtrace, variant, source, markers);
    // Registers hold the values of the innermost frame only; a caller's
    // were saved somewhere or lost.
    let innermost = backtrace
        .frames
        .iter()
        .find(|frame| frame.id == snapshot.stack_frame)
        .is_some_and(|frame| frame.level == 0);
    let shown = |name: &str| {
        snapshot
            .variables
            .iter()
            .find(|variable| &*variable.name == name)
            .map(|variable| &variable.state)
    };
    let mut marks = Vec::new();
    for evaluated in evaluations {
        let state = match &evaluated.result {
            Ok(crate::Evaluation::Value { value, .. }) => Ok(&value.state),
            Ok(other) => Err(format!("{other:?}")),
            Err(error) => Err(error.clone()),
        };
        let wrong = |expected: &str| {
            format!(
                "`{}` evaluated to {state:?}, not {expected}",
                evaluated.text
            )
        };
        match &evaluated.purpose {
            Purpose::Marker { .. } | Purpose::Expected => {
                let Some(marker) = marker else { continue };
                let expected = evaluated.purpose != Purpose::Marker { negated: true };
                match &state {
                    Ok(VariableState::Available {
                        value: VariableValue::Scalar(ScalarValue::Boolean(truth)),
                        ..
                    }) if *truth == expected => match evaluated.purpose {
                        Purpose::Expected => marks.push(Mark::ExpectationHeld),
                        Purpose::Marker { negated: false } => marks.push(Mark::MarkerEvaluated),
                        _ => {}
                    },
                    Ok(VariableState::Unavailable(_)) | Err(_) if variant.facts.optimized => {}
                    _ => {
                        let what = if evaluated.purpose == Purpose::Expected {
                            "what it expected, "
                        } else {
                            ""
                        };
                        return Err(format!(
                            "at line {} of {source}, {what}{}",
                            marker.line,
                            wrong(&expected.to_string())
                        ));
                    }
                }
            }
            Purpose::Name(name) => match (shown(name), &state) {
                (
                    Some(VariableState::Available { value: view, .. }),
                    Ok(state @ VariableState::Available { value, .. }),
                ) if value == view => {
                    marks.push(Mark::NameEvaluated);
                    marks.extend(stored(kernel, snapshot, innermost, &evaluated.text, state)?);
                }
                (
                    Some(VariableState::Unavailable(view)),
                    Ok(VariableState::Unavailable(reason)),
                ) if reason == view => {}
                (view, _) => return Err(wrong(&format!("{view:?}, as the variables view shows"))),
            },
            Purpose::Address(name) => match (shown(name), &state) {
                (
                    Some(VariableState::Available {
                        source: VariableValueSource::Memory(view),
                        ..
                    }),
                    Ok(VariableState::Available {
                        value: VariableValue::Address(address),
                        ..
                    }),
                ) if address.address == *view => marks.push(Mark::AddressEvaluated),
                (view, _) => return Err(wrong(&format!("the address of {view:?}"))),
            },
            Purpose::Cast { .. } | Purpose::IllTyped | Purpose::Arithmetic { .. } => {
                marks.extend(computed(evaluated, &state, shown)?);
            }
        }
    }
    Ok(marks)
}

/// Judges an expression computed from integer variables against the
/// values the variables view shows them holding.
fn computed<'v>(
    evaluated: &Evaluated,
    state: &Result<&VariableState, String>,
    shown: impl Fn(&str) -> Option<&'v VariableState>,
) -> Result<Option<Mark>, String> {
    let wrong = |expected: &str| {
        format!(
            "`{}` evaluated to {state:?}, not {expected}",
            evaluated.text
        )
    };
    match &evaluated.purpose {
        Purpose::Cast { name, bits, signed } => {
            let Some(value) = shown(name).and_then(integer) else {
                return Err(format!("`{}` casts a variable not shown", evaluated.text));
            };
            let low = value.cast_unsigned() & ((1 << bits) - 1);
            let truncated = if *signed && low >> (bits - 1) == 1 {
                low.cast_signed() - (1 << bits)
            } else {
                low.cast_signed()
            };
            match state {
                Ok(state) if integer(state) == Some(truncated) => Ok(Some(Mark::CastEvaluated)),
                _ => Err(wrong(&format!(
                    "{truncated}, {value} truncated to {bits} bits"
                ))),
            }
        }
        Purpose::IllTyped => match state {
            Err(_) => Ok(Some(Mark::IllTypedRefused)),
            Ok(_) => Err(format!(
                "`{}` is ill-typed, but evaluated to {state:?}",
                evaluated.text
            )),
        },
        Purpose::Arithmetic {
            left,
            operator,
            right,
        } => {
            let (Some(a), Some(b)) = (
                shown(left).and_then(integer),
                shown(right).and_then(integer),
            ) else {
                return Err(format!("`{}` combines variables not shown", evaluated.text));
            };
            let exact = match operator {
                '+' => a.checked_add(b),
                '-' => a.checked_sub(b),
                _ => a.checked_mul(b),
            };
            let Some(exact) = exact else { return Ok(None) };
            match state {
                Ok(state) if integer(state) == Some(exact) => Ok(Some(Mark::ArithmeticEvaluated)),
                _ => Err(wrong(&format!("{exact}, the exact result"))),
            }
        }
        _ => unreachable!("only computed expressions are judged here"),
    }
}

/// Storage truth: bytes the debugger says it read from memory, or from a
/// register of the innermost frame, are what the simulated machine holds
/// there.
fn stored(
    kernel: &Kernel,
    snapshot: &VariableSnapshot,
    innermost: bool,
    text: &str,
    state: &VariableState,
) -> Result<Option<Mark>, String> {
    let (
        VariableState::Available {
            source,
            raw: Some(raw),
            ..
        },
        Some(thread),
    ) = (
        state,
        snapshot
            .context
            .as_thread()
            .and_then(|thread| stopped(kernel, thread)),
    )
    else {
        return Ok(None);
    };
    let (place, held, mark) = match source {
        VariableValueSource::Memory(address) => (
            address.to_string(),
            kernel.processes[&thread.tgid]
                .space
                .read_user(address.get(), raw.len() as u64),
            Mark::StorageTrue,
        ),
        VariableValueSource::Register(register) if innermost && raw.len() <= 8 => {
            let Some(index) = GENERAL_REGISTERS
                .iter()
                .position(|name| *name == &*register.name)
            else {
                return Ok(None);
            };
            let bytes = thread.registers.general[index].to_le_bytes();
            (
                register.name.to_string(),
                Some(bytes[..raw.len()].to_vec()),
                Mark::RegisterTrue,
            )
        }
        _ => return Ok(None),
    };
    if held.as_deref() != Some(&**raw) {
        return Err(format!(
            "`{text}` showed bytes {raw:?} from {place}, which holds {held:?}"
        ));
    }
    Ok(Some(mark))
}

/// The simulated CPU's general registers, in its order.
const GENERAL_REGISTERS: [&str; 16] = [
    "rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13",
    "r14", "r15",
];

/// The marker whose condition must hold where the variables were read:
/// the thread stands at the start of a row of the marker's line, and the
/// debugger presents that line in its innermost frame.
fn marker_at<'m>(
    kernel: &Kernel,
    snapshot: &VariableSnapshot,
    backtrace: &Backtrace,
    variant: &Variant,
    source: &str,
    markers: &'m [Marker],
) -> Option<&'m Marker> {
    let thread = stopped(kernel, snapshot.context.as_thread()?)?;
    let facts = &variant.facts;
    let line = thread
        .registers
        .rip
        .checked_sub(variant.image.bias())
        .and_then(|image| facts.range(image))
        .filter(|range| range.start + variant.image.bias() == thread.registers.rip)
        .and_then(|range| range.line)
        .filter(|line| facts.file(*line) == source)?;
    let marker = markers.iter().find(|marker| marker.line == line.line)?;
    backtrace
        .frames
        .iter()
        .find(|frame| frame.id == snapshot.stack_frame)
        .is_some_and(|frame| {
            frame.level == 0
                && frame.source.as_ref().map(|source| source.line.get()) == Some(line.line)
        })
        .then_some(marker)
}

/// An available integer value.
fn integer(state: &VariableState) -> Option<i128> {
    let VariableState::Available { value, .. } = state else {
        return None;
    };
    match value {
        VariableValue::Scalar(ScalarValue::Signed(value)) => Some(*value),
        VariableValue::Scalar(ScalarValue::Unsigned(value)) => i128::try_from(*value).ok(),
        VariableValue::Enumeration { value, .. } => match value {
            crate::IntegerValue::Signed(value) => Some(*value),
            crate::IntegerValue::Unsigned(value) => i128::try_from(*value).ok(),
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::sim::corpus::Corpus;
    use crate::sim::kernel::shadow::Call;
    use crate::{StackFrame, ThreadId, VirtualAddress};

    /// A stopped thread of `variant` at its entry, `calls` deep, with each
    /// call's return address on its stack.
    fn kernel_with_calls(variant: &Variant, calls: &[u64]) -> (Kernel, Tid) {
        let mut kernel = Kernel::new(100);
        let tid = kernel.spawn(Arc::clone(&variant.image), &variant.path, &[], [0; 16]);
        let rsp = kernel.threads[&tid].registers.general[crate::sim::cpu::RSP];
        for (index, &return_address) in calls.iter().enumerate() {
            let slot = rsp - 64 * (index as u64 + 1);
            kernel
                .poke(tid, slot, return_address)
                .expect("write the stack");
            let thread = kernel.threads.get_mut(&tid).expect("the thread");
            thread.shadow.calls.push(Call {
                return_address,
                slot,
                activation: 10 + index as u64,
            });
        }
        (kernel, tid)
    }

    fn backtrace_of(tid: Tid, frames: &[u64], termination: UnwindTermination) -> Backtrace {
        Backtrace {
            context: ThreadId::new(u64::try_from(tid).expect("a positive tid")).into(),
            frames: frames
                .iter()
                .enumerate()
                .map(|(level, &address)| {
                    StackFrame::new(
                        u32::try_from(level).expect("few frames"),
                        FrameKind::Physical,
                        None,
                        VirtualAddress::new(address),
                    )
                })
                .collect(),
            termination,
        }
    }

    /// A backtrace shows the thread's calls innermost first. It may stop
    /// early only by saying why, and it may show a caller read from an
    /// overwritten return address only as its last frame.
    #[test]
    fn backtraces_show_every_call_or_say_why_not() {
        let corpus = Corpus::load().expect("load the golden corpus");
        let variant = &corpus.programs[0].variants[0];
        let (mut kernel, tid) = kernel_with_calls(variant, &[0x40_1111, 0x40_2222]);
        let rip = kernel.threads[&tid].registers.rip;
        let stopped = UnwindTermination::NoUnwindInfo {
            address: VirtualAddress::new(0x40_2222),
        };
        let judge = |kernel: &Kernel, frames: &[u64], termination: UnwindTermination| {
            backtrace(kernel, &backtrace_of(tid, frames, termination))
        };
        let complete = UnwindTermination::Complete;
        assert_eq!(
            judge(&kernel, &[rip, 0x40_2222, 0x40_1111], complete.clone()),
            Ok(Some(Unwound::Whole))
        );
        assert_eq!(
            judge(&kernel, &[rip, 0x40_2222], stopped.clone()),
            Ok(Some(Unwound::Truncated))
        );
        assert!(judge(&kernel, &[rip, 0x40_2222], complete.clone()).is_err());
        assert!(judge(&kernel, &[rip, 0x40_2223, 0x40_1111], complete.clone()).is_err());
        assert!(
            judge(
                &kernel,
                &[rip, 0x40_2222, 0x40_1111, 0x40_3333],
                complete.clone()
            )
            .is_err()
        );
        assert!(judge(&kernel, &[rip, 0x40_2222, 0x40_1111], stopped.clone()).is_err());

        // The program overwrites the outer call's return address.
        let slot = kernel.threads[&tid].shadow.calls[0].slot;
        kernel.poke(tid, slot, 1).expect("write the stack");
        assert_eq!(
            judge(&kernel, &[rip, 0x40_2222, 1], stopped.clone()),
            Ok(Some(Unwound::Corrupt))
        );
        assert!(judge(&kernel, &[rip, 0x40_2222, 1], complete).is_err());
        assert!(judge(&kernel, &[rip, 0x40_2222, 0x40_1111], stopped.clone()).is_err());
        assert!(judge(&kernel, &[rip, 0x40_2222, 1, 0x40_4444], stopped).is_err());
    }

    /// In unoptimized code, a source step stops at the first start of a
    /// statement of another line it reaches in its frame, not later.
    #[test]
    fn source_steps_stop_at_the_first_line_they_reach() {
        let corpus = Corpus::load().expect("load the golden corpus");
        let program = corpus
            .programs
            .iter()
            .find(|program| program.name == "straight")
            .expect("the straight program");
        let variant = program
            .variants
            .iter()
            .find(|variant| variant.name == "straight-gcc-O0")
            .expect("its unoptimized variant");
        let facts = &variant.facts;
        let main = facts
            .functions
            .iter()
            .find(|function| function.name == "main")
            .expect("main");
        // Three statement rows of distinct lines in main, in address order.
        let mut lines = Vec::new();
        for range in facts
            .ranges
            .iter()
            .filter(|range| range.statement && (main.start..main.end).contains(&range.start))
        {
            if range.line.is_some()
                && lines
                    .last()
                    .is_none_or(|last: &facts::Range| last.line != range.line)
            {
                lines.push(*range);
            }
        }
        let [first, second, third, ..] = lines[..] else {
            panic!("main has three lines");
        };
        let (mut kernel, tid) = kernel_with_calls(variant, &[0x40_1111]);
        let begun = {
            let thread = kernel.threads.get_mut(&tid).expect("the thread");
            thread.registers.rip = first.start;
            Begun::new(thread, StepKind::OverSource, None)
        };
        let activation = begun.shadow.activation();
        {
            let thread = kernel.threads.get_mut(&tid).expect("the thread");
            thread.registers.rip = third.start;
            thread.retired += 3;
        }
        let passed = |rip| Position {
            rip,
            depth: 1,
            activation,
        };
        let judge = |positions: &[Position]| step(&kernel, &begun, positions, variant);
        assert_eq!(
            judge(&[passed(first.start), passed(second.start - 1)]),
            Ok(Some(Judged::Exactly))
        );
        let past = judge(&[passed(first.start), passed(second.start)]);
        assert!(
            past.as_ref()
                .is_err_and(|message| message.contains("ran past")),
            "{past:?}"
        );
    }
}
