//! Running a bound view at one stop. A view reaches the program only
//! through the evaluator's [`Machine`], which charges the inspection's
//! budget for every read and every evaluated part, so a presentation's cost
//! is bounded and the same inputs always stop at the same point.

use std::sync::Arc;

use crate::eval::interp::{self, Outcome, Value};
use crate::eval::number::{Exact, Integer};
use crate::eval::target::{Machine, Refusal, Register, Stop};
use crate::eval::types::{Category, TypeSource, category};
use crate::{
    InspectedValue, PresentedShape, TextCompletion, TextSummary, TypeInfo, TypeReference,
    ValueAccessUnavailableReason, VariableState, VariableUnavailableReason, VariableValue,
    ViewProblem,
};

use super::bind::{BoundShape, BoundView, TextSource, ViewObject, ViewProgram};
use super::summary;

/// The size of the pages text is read in, which a read never crosses, so
/// an unmapped page ends the text rather than failing what came before.
const PAGE_SIZE: u64 = 4096;

/// Why a view could not present a value.
#[derive(Debug)]
pub enum Failure {
    /// The value shows as it is stored, with this problem beside it.
    Problem(ViewProblem),
    /// The debugger failed, which fails the whole inspection.
    Debugger(crate::Error),
}

impl From<Stop> for Failure {
    fn from(stop: Stop) -> Self {
        match stop {
            Stop::Missing(state) => Self::Problem(match *state {
                VariableState::Unavailable(reason) => ViewProblem::Unavailable(reason),
                VariableState::Malformed(reason) => ViewProblem::Refused(reason.description),
                VariableState::Invalid { reason, .. } => {
                    ViewProblem::Refused(reason.to_string().into())
                }
                VariableState::Available { .. } => {
                    ViewProblem::Internal("an available value stopped the view".into())
                }
            }),
            Stop::Refused(refusal) => Self::Problem(ViewProblem::Refused(refusal.message.into())),
            Stop::Failed(error) => Self::Debugger(error),
        }
    }
}

impl From<interp::Failure> for Failure {
    fn from(failure: interp::Failure) -> Self {
        match failure {
            interp::Failure::Expression(error) => {
                Self::Problem(ViewProblem::Refused(error.message.into()))
            }
            interp::Failure::Debugger(error) => Self::Debugger(error),
        }
    }
}

fn internal(message: &str) -> Failure {
    Failure::Problem(ViewProblem::Internal(message.into()))
}

/// What running a view presented.
#[derive(Debug)]
pub struct Presented {
    pub shape: PresentedShape,
    /// How many elements a sequence holds.
    pub count: Option<u64>,
    pub text: Option<TextSummary>,
    pub summary: String,
    /// For `value`, the value that stands for this one.
    pub inner: Option<InspectedValue>,
    /// Why the summary stopped short of every element.
    pub partial: Option<ViewProblem>,
}

/// One child of a presented value.
#[derive(Debug)]
pub enum Child {
    Element(u64, InspectedValue),
    Field(Arc<str>, InspectedValue),
    /// The value as it is stored, which the caller materializes.
    Raw,
}

/// A machine that answers a view's names: `self` and its members, its
/// `let`s, each computed once, and its generators' variables.
struct ViewMachine<'m, M: Machine> {
    base: &'m mut M,
    this: M::Place,
    lets: &'m [super::bind::BoundLet<M::Step>],
    values: Vec<Option<Value<M::Place>>>,
    variables: Vec<i128>,
}

impl<'m, M: Machine> ViewMachine<'m, M> {
    fn new(base: &'m mut M, bound: &'m BoundView<M::Step>, this: M::Place) -> Self {
        Self {
            base,
            this,
            lets: &bound.lets,
            values: vec![None; bound.lets.len()],
            variables: Vec::new(),
        }
    }

    fn let_value(&mut self, index: usize) -> Result<Value<M::Place>, Stop> {
        if let Some(Some(value)) = self.values.get(index) {
            return Ok(value.clone());
        }
        let lets = self.lets;
        let program = &lets
            .get(index)
            .ok_or_else(|| {
                Stop::Refused(Refusal::new(
                    crate::ExpressionErrorKind::Unsupported,
                    "no such `let`",
                ))
            })?
            .program;
        let value = interp::value(program, self)?;
        self.values[index] = Some(value.clone());
        Ok(value)
    }
}

impl<M: Machine> TypeSource for ViewMachine<'_, M> {
    fn type_info(&self, ty: TypeReference) -> Option<TypeInfo> {
        self.base.type_info(ty)
    }

    fn pointer_size(&self) -> u8 {
        self.base.pointer_size()
    }

    fn byte_order(&self) -> crate::ByteOrder {
        self.base.byte_order()
    }
}

impl<M: Machine> Machine for ViewMachine<'_, M> {
    type Object = ViewObject<M::Step>;
    type Step = M::Step;
    type Place = M::Place;

    fn charge(&mut self) -> Result<(), Stop> {
        self.base.charge()
    }

    fn locate(&mut self, object: &Self::Object) -> Result<Self::Place, Stop> {
        match object {
            ViewObject::This => Ok(self.this.clone()),
            ViewObject::Member(step) => {
                let this = self.this.clone();
                self.base.step(&this, step, &[])
            }
            ViewObject::Let(index) => match self.let_value(*index)? {
                Value::Place(place) => Ok(place),
                _ => Err(Stop::Refused(Refusal::new(
                    crate::ExpressionErrorKind::Unsupported,
                    "the `let` is not a place",
                ))),
            },
            ViewObject::Variable(_) => Err(Stop::Refused(Refusal::new(
                crate::ExpressionErrorKind::Unsupported,
                "a generator's variable is not a place",
            ))),
        }
    }

    fn check_indices(&self, step: &Self::Step, indices: &[i128]) -> Result<(), Stop> {
        self.base.check_indices(step, indices)
    }

    fn step(
        &mut self,
        from: &Self::Place,
        step: &Self::Step,
        indices: &[i128],
    ) -> Result<Self::Place, Stop> {
        self.base.step(from, step, indices)
    }

    fn place_at(&mut self, address: u64, ty: TypeReference) -> Result<Self::Place, Stop> {
        self.base.place_at(address, ty)
    }

    fn address(&self, at: &Self::Place) -> Result<u64, Stop> {
        self.base.address(at)
    }

    fn load(&mut self, at: &Self::Place) -> Result<VariableValue, Stop> {
        self.base.load(at)
    }

    fn read(&mut self, address: u64, size: usize) -> Result<Vec<u8>, Stop> {
        self.base.read(address, size)
    }

    fn text(&mut self, at: &Self::Place) -> Result<Option<TextSummary>, Stop> {
        self.base.text(at)
    }

    fn length(&mut self, at: &Self::Place) -> Result<u64, Stop> {
        self.base.length(at)
    }

    fn presented_length(&mut self, at: &Self::Place) -> Result<Option<u64>, Stop> {
        self.base.presented_length(at)
    }

    fn register(&mut self, _register: &Register) -> Result<u128, Stop> {
        Err(Stop::Refused(Refusal::new(
            crate::ExpressionErrorKind::Unsupported,
            "views read no registers",
        )))
    }

    fn present(&mut self, at: &Self::Place) -> Result<InspectedValue, Stop> {
        self.base.present(at)
    }

    fn present_bytes(&mut self, ty: TypeReference, bytes: &[u8]) -> Result<InspectedValue, Stop> {
        self.base.present_bytes(ty, bytes)
    }

    fn present_pointer(
        &mut self,
        address: u64,
        pointee: Option<TypeReference>,
        type_info: TypeInfo,
    ) -> Result<InspectedValue, Stop> {
        self.base.present_pointer(address, pointee, type_info)
    }

    fn finish(&self, type_info: Option<TypeInfo>, state: VariableState) -> InspectedValue {
        self.base.finish(type_info, state)
    }

    fn bound(&mut self, object: &Self::Object) -> Result<Value<Self::Place>, Stop> {
        match object {
            ViewObject::Let(index) => self.let_value(*index),
            ViewObject::Variable(depth) => {
                let value = self.variables.get(*depth).copied().ok_or_else(|| {
                    Stop::Refused(Refusal::new(
                        crate::ExpressionErrorKind::Unsupported,
                        "the generator's variable is out of scope",
                    ))
                })?;
                Ok(Value::Int(Integer::Exact(Exact::from(value))))
            }
            ViewObject::This | ViewObject::Member(_) => Err(Stop::Refused(Refusal::new(
                crate::ExpressionErrorKind::Unsupported,
                "a place is not a bound value",
            ))),
        }
    }
}

fn truth<M: Machine>(
    program: &ViewProgram<M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<bool, Failure> {
    match interp::value(program, machine)? {
        Value::Bool(value) => Ok(value),
        _ => Err(internal("a condition is not a truth value")),
    }
}

fn exact<M: Machine>(
    program: &ViewProgram<M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<Exact, Failure> {
    match interp::value(program, machine)? {
        Value::Int(integer) => Ok(integer.value()),
        _ => Err(internal("a count is not an integer")),
    }
}

/// A count: a non-negative integer.
fn count<M: Machine>(
    program: &ViewProgram<M::Step>,
    machine: &mut ViewMachine<'_, M>,
    what: &str,
) -> Result<u64, Failure> {
    let value = exact(program, machine)?;
    value
        .to_u128()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| {
            Failure::Problem(ViewProblem::Refused(
                format!("the {what} is {value}").into(),
            ))
        })
}

/// A value of a check's side, for saying why it failed.
fn side_text<P>(value: &Value<P>) -> String {
    match value {
        Value::Int(integer) => integer.value().to_string(),
        Value::Float(float) => float.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Pointer(address) | Value::Raw(address) => format!("{address:#x}"),
        Value::Place(_) | Value::Text(_) => "…".to_owned(),
    }
}

/// Runs the view's checks, failing at the first that does not hold.
fn checks<M: Machine>(
    bound: &BoundView<M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<(), Failure> {
    for check in &bound.checks {
        if truth(&check.program, machine)? {
            continue;
        }
        let detail = check.sides.as_ref().and_then(|sides| {
            let mut parts = Vec::new();
            for (text, program) in sides {
                let value = interp::value(program, machine).ok()?;
                parts.push(format!("`{text}` is {}", side_text(&value)));
            }
            Some(Arc::from(parts.join(", ")))
        });
        return Err(Failure::Problem(ViewProblem::CheckFailed {
            check: check.text.as_str().into(),
            detail,
        }));
    }
    Ok(())
}

/// The shape a value has, choosing `if` branches.
fn resolve<'b, M: Machine>(
    shape: &'b BoundShape<M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<&'b BoundShape<M::Step>, Failure> {
    let mut shape = shape;
    while let BoundShape::If {
        condition,
        then,
        otherwise,
    } = shape
    {
        shape = if truth(condition, machine)? {
            then
        } else {
            otherwise
        };
    }
    Ok(shape)
}

/// How many elements a sequence generates, refusing a count that disagrees.
fn sequence_length<M: Machine>(
    count_program: Option<&ViewProgram<M::Step>>,
    length: &ViewProgram<M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<u64, Failure> {
    let generated = count(length, machine, "range's length")?;
    if let Some(program) = count_program {
        let declared = count(program, machine, "count")?;
        if declared != generated {
            return Err(Failure::Problem(ViewProblem::CountMismatch {
                declared,
                generated,
            }));
        }
    }
    Ok(generated)
}

/// Presents one value, the result of a program, as inspection presents it.
fn run_value<M: Machine>(
    program: &ViewProgram<M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<InspectedValue, Failure> {
    match interp::run(program, machine)? {
        Outcome::Value { value, .. } => Ok(value),
        _ => Err(Failure::Problem(ViewProblem::Refused(
            "a view's value is one value".into(),
        ))),
    }
}

/// Element `index`. An element the view cannot compute, such as one past
/// the address space, is that element's problem, not the sequence's.
fn element<M: Machine>(
    program: &ViewProgram<M::Step>,
    machine: &mut ViewMachine<'_, M>,
    index: u64,
) -> Result<InspectedValue, Failure> {
    machine.variables.push(i128::from(index));
    let value = run_value(program, machine);
    machine.variables.pop();
    match value {
        Err(Failure::Problem(ViewProblem::Unavailable(reason))) => {
            Ok(machine.finish(None, VariableState::Unavailable(reason)))
        }
        Err(Failure::Problem(problem)) => Ok(machine.finish(
            None,
            VariableState::Malformed(crate::VariableMalformedReason {
                kind: crate::VariableMalformedKind::InvalidExpression,
                description: problem.to_string().into(),
            }),
        )),
        value => value,
    }
}

/// Presents `this`, a value of the view's type, as the view says.
pub fn present<M: Machine>(
    bound: &BoundView<M::Step>,
    machine: &mut M,
    this: M::Place,
) -> Result<Presented, Failure> {
    let mut machine = ViewMachine::new(machine, bound, this);
    checks(bound, &mut machine)?;
    let mut presented = match resolve(&bound.shape, &mut machine)? {
        BoundShape::Text { source, length } => {
            let text = read_text(source, length.as_ref(), &mut machine)?;
            Presented {
                shape: PresentedShape::Text,
                count: None,
                summary: summary::quoted(&text),
                text: Some(text),
                inner: None,
                partial: None,
            }
        }
        BoundShape::Value(program) => {
            let inner = run_value(program, &mut machine)?;
            Presented {
                shape: PresentedShape::Value,
                count: None,
                text: None,
                summary: summary::value(inner.type_info.as_ref(), &inner.state),
                inner: Some(inner),
                partial: None,
            }
        }
        BoundShape::Empty(text) => Presented {
            shape: PresentedShape::Empty,
            count: None,
            text: None,
            summary: text.to_string(),
            inner: None,
            partial: None,
        },
        BoundShape::Sequence {
            count: declared,
            length,
            element: program,
        } => {
            let length = sequence_length(declared.as_ref(), length, &mut machine)?;
            let mut elements = Vec::new();
            let mut characters = 0;
            let mut partial = None;
            for index in 0..length {
                if elements.len() == summary::MAX_ELEMENTS || characters >= summary::MAX_CHARACTERS
                {
                    break;
                }
                let value = element(program, &mut machine, index)?;
                let rendered = summary::value(value.type_info.as_ref(), &value.state);
                characters += rendered.chars().count() + 2;
                elements.push(rendered);
                // An element the budget or memory could not provide ends
                // the preview, rather than spending more on the rest.
                match value.state {
                    VariableState::Unavailable(reason) => {
                        partial = Some(ViewProblem::Unavailable(reason));
                        break;
                    }
                    VariableState::Malformed(reason) => {
                        partial = Some(ViewProblem::Refused(reason.description));
                        break;
                    }
                    _ => {}
                }
            }
            let complete = elements.len() as u64 == length;
            Presented {
                shape: PresentedShape::Sequence,
                count: Some(length),
                text: None,
                summary: summary::sequence(length, &elements, complete),
                inner: None,
                partial,
            }
        }
        BoundShape::If { .. } => unreachable!("`if` is resolved"),
    };
    if let Some(pieces) = &bound.summary {
        let mut text = String::new();
        for piece in pieces {
            match piece {
                super::bind::BoundPiece::Literal(literal) => text.push_str(literal),
                super::bind::BoundPiece::Hole(program) => {
                    let value = run_value(program, &mut machine)?;
                    text.push_str(&summary::value(value.type_info.as_ref(), &value.state));
                }
            }
        }
        presented.summary = text;
    }
    Ok(presented)
}

/// The children `offset..offset + limit` of `this` as the view presents
/// it: its elements, then its fields, then the stored value.
pub fn children<M: Machine>(
    bound: &BoundView<M::Step>,
    machine: &mut M,
    this: M::Place,
    offset: u64,
    limit: u64,
) -> Result<Vec<Child>, Failure> {
    let mut machine = ViewMachine::new(machine, bound, this);
    checks(bound, &mut machine)?;
    let shape = resolve(&bound.shape, &mut machine)?;
    let (elements, program) = match shape {
        BoundShape::Sequence {
            count: declared,
            length,
            element,
        } => (
            sequence_length(declared.as_ref(), length, &mut machine)?,
            Some(element),
        ),
        _ => (0, None),
    };
    let fields = bound.fields.len() as u64;
    let end = offset
        .saturating_add(limit)
        .min(elements.saturating_add(fields).saturating_add(1));
    let mut children = Vec::new();
    for index in offset..end {
        let child = if index < elements {
            let program = program.expect("a sequence has an element");
            Child::Element(index, element(program, &mut machine, index)?)
        } else if let Some(field) = bound
            .fields
            .get(usize::try_from(index - elements).unwrap_or(usize::MAX))
        {
            Child::Field(
                Arc::clone(&field.name),
                run_value(&field.program, &mut machine)?,
            )
        } else {
            Child::Raw
        };
        // A page ends at the first child its budget cannot afford, which
        // the page's completion then reports.
        if let Child::Element(_, value) | Child::Field(_, value) = &child
            && matches!(
                value.state,
                VariableState::Unavailable(VariableUnavailableReason::InspectionLimit(_))
            )
        {
            break;
        }
        children.push(child);
    }
    Ok(children)
}

/// How many elements `this` holds as the view presents it, or the length
/// of its text, for `len(v)`.
pub fn length<M: Machine>(
    bound: &BoundView<M::Step>,
    machine: &mut M,
    this: M::Place,
) -> Result<u64, Failure> {
    let mut machine = ViewMachine::new(machine, bound, this);
    checks(bound, &mut machine)?;
    match resolve(&bound.shape, &mut machine)? {
        BoundShape::Sequence {
            count: declared,
            length,
            ..
        } => sequence_length(declared.as_ref(), length, &mut machine),
        BoundShape::Text { source, length } => {
            let text = read_text(source, length.as_ref(), &mut machine)?;
            match text.completion {
                TextCompletion::Complete => Ok(text.bytes.len() as u64),
                TextCompletion::Truncated {
                    length: Some(length),
                }
                | TextCompletion::Limited {
                    length: Some(length),
                    ..
                } => Ok(length),
                _ => Err(Failure::Problem(ViewProblem::Refused(
                    "the text's length could not be read".into(),
                ))),
            }
        }
        _ => Err(Failure::Problem(ViewProblem::Refused(
            "the view presents no elements".into(),
        ))),
    }
}

/// The place of element `index` of `this`, for `v[i]`.
pub fn element_place<M: Machine>(
    bound: &BoundView<M::Step>,
    machine: &mut M,
    this: M::Place,
    index: i128,
) -> Result<M::Place, Failure> {
    let mut machine = ViewMachine::new(machine, bound, this);
    checks(bound, &mut machine)?;
    let BoundShape::Sequence {
        count: declared,
        length,
        element,
    } = resolve(&bound.shape, &mut machine)?
    else {
        return Err(Failure::Problem(ViewProblem::Refused(
            "the view presents no elements".into(),
        )));
    };
    let length = sequence_length(declared.as_ref(), length, &mut machine)?;
    if index < 0 || index >= i128::from(length) {
        return Err(Failure::Problem(ViewProblem::Unavailable(
            VariableUnavailableReason::IndexOutOfBounds {
                index,
                lower_bound: 0,
                count: length,
            },
        )));
    }
    machine.variables.push(index);
    let value = interp::value(element, &mut machine);
    machine.variables.pop();
    match value? {
        Value::Place(place) => Ok(place),
        _ => Err(Failure::Problem(ViewProblem::Refused(
            "the view computes its elements, which have no place".into(),
        ))),
    }
}

/// The text a `text` shape presents, at most [`TextSummary::MAX_BYTES`].
fn read_text<M: Machine>(
    source: &TextSource<M::Step>,
    length: Option<&ViewProgram<M::Step>>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<TextSummary, Failure> {
    let declared = length
        .map(|program| count(program, machine, "text's length"))
        .transpose()?;
    let (address, length) = match source {
        TextSource::Pointer(program) => match interp::value(program, machine)? {
            Value::Pointer(address) => (address, declared),
            _ => return Err(internal("a text's pointer is not a pointer")),
        },
        TextSource::Elements { program, first } => {
            let Value::Place(place) = interp::value(program, machine)? else {
                return Err(internal("a text's elements are not a place"));
            };
            let available = match category(machine, program.result()) {
                Category::Array { dimensions, .. } => {
                    dimensions.first().map_or(0, |dimension| dimension.count)
                }
                _ => machine.length(&place)?,
            };
            let length = declared.unwrap_or(available);
            if length > available {
                return Err(Failure::Problem(ViewProblem::Refused(
                    format!("the text's length {length} exceeds its {available} elements").into(),
                )));
            }
            if length == 0 {
                return Ok(TextSummary {
                    bytes: Arc::from([]),
                    completion: TextCompletion::Complete,
                });
            }
            let first = machine.step(&place, first, &[0])?;
            (machine.address(&first)?, Some(length))
        }
    };
    if address == 0 && length != Some(0) {
        return Err(Failure::Problem(ViewProblem::Unavailable(
            VariableUnavailableReason::ValueAccess(ValueAccessUnavailableReason::NullPointer),
        )));
    }
    read_bytes(machine, address, length)
}

/// Reads text at `address`, `length` bytes or up to a NUL, a page at a
/// time, at most [`TextSummary::MAX_BYTES`].
fn read_bytes<M: Machine>(
    machine: &mut M,
    address: u64,
    length: Option<u64>,
) -> Result<TextSummary, Failure> {
    let limit = TextSummary::MAX_BYTES as u64;
    let wanted = length.map_or(limit, |length| length.min(limit));
    let mut bytes = Vec::new();
    let stopped = loop {
        let read = bytes.len() as u64;
        if read == wanted {
            break None;
        }
        let Some(next) = address.checked_add(read) else {
            break None;
        };
        let size = (PAGE_SIZE - next % PAGE_SIZE).min(wanted - read);
        let chunk = match machine.read(next, usize::try_from(size).unwrap_or(usize::MAX)) {
            Ok(chunk) => chunk,
            Err(Stop::Missing(state)) => {
                break Some(match *state {
                    VariableState::Unavailable(VariableUnavailableReason::InspectionLimit(
                        exhaustion,
                    )) => TextCompletion::Limited { length, exhaustion },
                    _ => TextCompletion::Unreadable {
                        address: crate::VirtualAddress::new(next),
                    },
                });
            }
            Err(stop) => return Err(stop.into()),
        };
        if length.is_none()
            && let Some(position) = chunk.iter().position(|byte| *byte == 0)
        {
            bytes.extend_from_slice(&chunk[..position]);
            return Ok(TextSummary {
                bytes: bytes.into(),
                completion: TextCompletion::Complete,
            });
        }
        bytes.extend_from_slice(&chunk);
    };
    let completion = match (stopped, length) {
        (Some(completion), _) => completion,
        (None, Some(length)) if bytes.len() as u64 == length => TextCompletion::Complete,
        (None, length) => TextCompletion::Truncated { length },
    };
    Ok(TextSummary {
        bytes: bytes.into(),
        completion,
    })
}
