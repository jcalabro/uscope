//! Views at a stop: which view presents each type, the presentation every
//! inspection path attaches to the values it returns, and the children of
//! presented values (`plans/views.md` §3.11).
//!
//! Views run here, on the controller thread, inside the inspection that
//! asked for the value, charged to its budget. A view binds once per type
//! and view set; each presentation then reads only what it shows.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::debug_info::Located;
use crate::eval::target::{
    Lookup, Planned, Refusal, Register, Scope, StepKind, Stop, TypeLookup, TypeQuery,
};
use crate::eval::types::{Ty, TypeSource};
use crate::inspection::InspectionBudget;
use crate::model::ViewChildren;
use crate::view::bind::BoundShape;
use crate::view::run::{Child, Failure};
use crate::view::{Choice, ViewSet};
use crate::{
    ByteOrder, Error, InspectedValue, PointerWidth, Presentation, PresentedCount, PresentedShape,
    Result, TypeInfo, TypeReference, ValueChild, ValueChildPage, ValueChildRelationship,
    ValueChildren, ValueChildrenReference, VariableState, ViewProblem,
};

use super::evaluation::{
    StopMachine, StopObject, StopPlace, StopStep, ViewBound, lookup_type_in, plan_in,
};
use super::native::InspectionOps;
use super::{Controller, RuntimeModule};

/// How deeply views present values inside the values they present.
const MAX_DEPTH: u8 = 4;

/// The views a controller presents values with, and the view each type
/// has.
pub(super) struct Views {
    pub(super) set: Arc<ViewSet>,
    pub(super) enabled: bool,
    /// The view each type has under this set, bound when first needed.
    choices: RefCell<BTreeMap<TypeReference, Arc<Choice<StopStep>>>>,
}

impl Default for Views {
    fn default() -> Self {
        Self {
            set: ViewSet::built_in(),
            enabled: true,
            choices: RefCell::default(),
        }
    }
}

impl Views {
    /// Presents with another set from now on.
    pub(super) fn replace(&mut self, set: Arc<ViewSet>) {
        self.set = set;
        self.choices.get_mut().clear();
    }
}

/// The scope a view binds in: one module's types, and no names. A view
/// sees nothing a frame names, so it means the same at every stop.
struct ModuleScope<'a, P: InspectionOps> {
    controller: &'a Controller<P>,
    module: &'a RuntimeModule,
}

impl<P: InspectionOps> TypeSource for ModuleScope<'_, P> {
    fn type_info(&self, ty: TypeReference) -> Option<TypeInfo> {
        self.controller
            .modules
            .values()
            .find(|module| module.loaded.image == ty.image)?
            .image
            .type_info(ty)
            .cloned()
    }

    fn pointer_size(&self) -> u8 {
        match self.controller.module_image.target().pointer_width {
            PointerWidth::Bits32 => 4,
            PointerWidth::Bits64 => 8,
        }
    }

    fn byte_order(&self) -> ByteOrder {
        self.controller.module_image.target().byte_order
    }
}

impl<P: InspectionOps> Scope for ModuleScope<'_, P> {
    type Object = StopObject;
    type Step = StopStep;

    fn lookup(
        &self,
        _name: &str,
        _outermost: bool,
    ) -> std::result::Result<Lookup<StopObject>, Refusal> {
        Ok(Lookup::NotFound)
    }

    fn lookup_type(&self, query: &TypeQuery) -> TypeLookup {
        lookup_type_in(self.module, query)
    }

    fn plan(
        &self,
        from: TypeReference,
        step: StepKind<'_>,
    ) -> std::result::Result<Planned<StopStep>, Refusal> {
        plan_in(self.controller, from, step)
    }

    fn register(&self, _name: &str) -> Option<Register> {
        None
    }
}

/// The program type of a sequence's elements, when every branch that
/// presents one agrees.
fn element_type(shape: &BoundShape<StopStep>) -> Option<TypeReference> {
    match shape {
        BoundShape::Sequence { element, .. } => match element.result() {
            Ty::Program(reference) if element.is_place() => Some(*reference),
            _ => None,
        },
        BoundShape::If {
            then, otherwise, ..
        } => match (element_type(then), element_type(otherwise)) {
            (Some(left), Some(right)) if left == right => Some(left),
            (Some(found), None) if !otherwise.has_elements() => Some(found),
            (None, Some(found)) if !then.has_elements() => Some(found),
            _ => None,
        },
        _ => None,
    }
}

impl<P: InspectionOps> Controller<P> {
    /// The view that presents values of `ty`, with why each candidate
    /// before it did not bind.
    pub(super) fn view_choice(&self, ty: TypeReference) -> Arc<Choice<StopStep>> {
        if let Some(choice) = self.views.choices.borrow().get(&ty) {
            return Arc::clone(choice);
        }
        let choice = Arc::new(
            self.modules
                .values()
                .find(|module| module.loaded.image == ty.image)
                .map_or_else(Choice::default, |module| {
                    crate::view::choose(
                        &self.views.set,
                        ty,
                        &ModuleScope {
                            controller: self,
                            module,
                        },
                    )
                }),
        );
        self.views
            .choices
            .borrow_mut()
            .insert(ty, Arc::clone(&choice));
        choice
    }

    /// Why an expression's value is presented as it is.
    pub(super) fn explain_view(
        &self,
        stop_id: crate::StopId,
        pid: nix::unistd::Pid,
        frame: crate::StackFrameId,
        expression: &crate::Expression,
    ) -> Result<crate::ViewExplanation> {
        let evaluation = self.evaluate(
            stop_id,
            pid,
            frame,
            expression,
            crate::EvaluationMode::Read,
            crate::InspectionLimits::default(),
        )?;
        let crate::Evaluation::Value { value, .. } = evaluation else {
            return Err(Error::InvalidValueExpression(
                "a range of elements has no view; explain one element".into(),
            ));
        };
        let candidates = value.type_info.as_ref().map_or_else(Vec::new, |info| {
            self.view_choice(info.reference)
                .candidates
                .iter()
                .map(|candidate| crate::ViewCandidate {
                    view: Arc::clone(&candidate.name),
                    rejection: candidate
                        .rejection
                        .as_ref()
                        .map(|rejection| rejection.to_string().into()),
                })
                .collect()
        });
        let presentation = match &value.state {
            VariableState::Available { presentation, .. } => presentation.clone(),
            _ => None,
        };
        Ok(crate::ViewExplanation {
            type_info: value.type_info,
            enabled: self.views.enabled,
            candidates: candidates.into(),
            presentation,
        })
    }

    /// The step from a value of `from` to an element its view presents,
    /// for `v[i]` on a value with no indexing of its own.
    pub(super) fn view_index(&self, from: TypeReference) -> Option<Planned<StopStep>> {
        if !self.views.enabled {
            return None;
        }
        let bound = self.view_choice(from).bound.clone()?;
        let element = element_type(&bound.shape)?;
        Some(Planned {
            step: StopStep::Element(bound),
            result: Some(element),
            consumed: 1,
        })
    }

    /// The children a view presents: its elements, its fields, and the
    /// value as stored.
    pub(super) fn view_children(
        &self,
        reference: &ValueChildrenReference,
        view: &ViewChildren,
        offset: u64,
        limit: u32,
        budget: &mut InspectionBudget,
    ) -> Result<ValueChildPage> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let pid = super::debug_pid(reference.thread)?;
        let frame = self.resolve_frame(inferior, pid, reference.frame)?;
        let scope = self.frame_for(inferior, reference.stop_id, pid, &frame);
        let bound = Arc::clone(&view.bound)
            .downcast::<ViewBound>()
            .map_err(|_| {
                Error::InvalidValueExpression("the view belongs to another debugger".into())
            })?;
        let this = StopPlace {
            module: reference.module,
            located: Located {
                ty: reference.target_type,
                storage: reference.storage.clone(),
            },
        };
        let end = offset.saturating_add(u64::from(limit)).min(reference.total);
        let mut children = Vec::new();
        let mut completion_budget_exhausted = None;
        // A view that presents the value as another lends it that value's
        // children.
        let mut next = offset;
        if let Some(inner) = &view.inner
            && next < view.elements
        {
            let count = end.min(view.elements) - next;
            let page = self.value_children_with_budget(
                inner,
                &crate::ValueChildQuery {
                    offset: next,
                    limit: u32::try_from(count).unwrap_or(u32::MAX),
                },
                budget,
            )?;
            children.extend(page.children.iter().cloned());
            next = view.elements;
            if page.completion.exhaustion().is_some() {
                completion_budget_exhausted = page.completion.exhaustion();
            }
        }
        if completion_budget_exhausted.is_none() && next < end {
            let mut machine = StopMachine::new(&scope, budget, true);
            // With borrowed elements, the view's own children start at
            // its fields.
            let start = if view.inner.is_some() {
                next - view.elements
            } else {
                next
            };
            let presented =
                crate::view::run::children(&bound, &mut machine, this.clone(), start, end - next);
            let presented = match presented {
                Ok(presented) => presented,
                Err(Failure::Debugger(error)) => return Err(error),
                Err(Failure::Problem(ViewProblem::Unavailable(
                    crate::VariableUnavailableReason::InspectionLimit(exhaustion),
                ))) => {
                    completion_budget_exhausted = Some(exhaustion);
                    Vec::new()
                }
                Err(Failure::Problem(problem)) => {
                    return Err(Error::ViewFailed(problem.to_string().into()));
                }
            };
            for child in presented {
                children.push(match child {
                    Child::Element(index, value) => ValueChild {
                        relationship: ValueChildRelationship::Element { index },
                        type_info: value.type_info.unwrap_or_else(|| placeholder(&this)),
                        state: value.state,
                    },
                    Child::Field(name, value) => ValueChild {
                        relationship: ValueChildRelationship::Field { name },
                        type_info: value.type_info.unwrap_or_else(|| placeholder(&this)),
                        state: value.state,
                    },
                    Child::Raw => {
                        let module = machine.module(this.module).map_err(stopped)?;
                        let value = machine
                            .materialize(module, &this.located)
                            .map_err(stopped)?;
                        ValueChild {
                            relationship: ValueChildRelationship::Raw,
                            type_info: value.type_info.unwrap_or_else(|| placeholder(&this)),
                            state: value.state,
                        }
                    }
                });
            }
        }
        Ok(ValueChildPage {
            stop_id: reference.stop_id,
            offset,
            total: reference.total,
            children: children.into(),
            completion: completion_budget_exhausted.map_or_else(
                || budget.completion(),
                crate::InspectionCompletion::Truncated,
            ),
            usage: budget.usage(),
        })
    }
}

/// A type for a child whose own type is unknown.
fn placeholder(this: &StopPlace) -> TypeInfo {
    TypeInfo {
        reference: TypeReference {
            image: crate::eval::types::LANGUAGE_IMAGE,
            id: this.located.ty,
        },
        name: "<unknown type>".into(),
        byte_size: None,
        kind: crate::TypeKind::Unspecified,
        identity: None,
    }
}

/// A machine's stop as the debugger's error.
fn stopped(stop: Stop) -> Error {
    match stop {
        Stop::Failed(error) => error,
        Stop::Missing(state) => Error::ViewFailed(format!("{state:?}").into()),
        Stop::Refused(refusal) => Error::ViewFailed(refusal.message.into()),
    }
}

impl<P: InspectionOps> StopMachine<'_, '_, P> {
    /// `value` with its presentation, when a view presents its type. The
    /// view runs with a share of the budget, so a presentation that runs
    /// out never takes the rest of the inspection with it.
    #[expect(
        clippy::too_many_lines,
        reason = "pointers to text and presented values share one budgeted path"
    )]
    pub(super) fn presented(
        &mut self,
        mut value: InspectedValue,
    ) -> std::result::Result<InspectedValue, Stop> {
        let controller = self.frame.controller;
        if !controller.views.enabled || self.depth >= MAX_DEPTH {
            return Ok(value);
        }
        if let VariableState::Available {
            value: crate::VariableValue::Address(_),
            dereference:
                crate::DereferenceState::Available(crate::DereferenceReference {
                    module,
                    image,
                    target_type,
                    target: crate::model::DereferenceTarget::Address(address),
                    ..
                }),
            text: None,
            ..
        } = &value.state
        {
            // A pointer or reference to text shows the text, as a pointer
            // to characters does, or why its view could not read it. A
            // null pointer, like a null pointer to characters, points at
            // nothing to read.
            if address.get() == 0 {
                return Ok(value);
            }
            let target = TypeReference {
                image: *image,
                id: *target_type,
            };
            let place = StopPlace {
                module: *module,
                located: Located {
                    ty: *target_type,
                    storage: crate::model::ValueStorage::Memory(*address),
                },
            };
            let (text, presentation) = self.pointee_text(target, place)?;
            if let VariableState::Available {
                text: state_text,
                presentation: state_presentation,
                ..
            } = &mut value.state
            {
                *state_text = text.map(Arc::new);
                *state_presentation = presentation.map(Arc::new);
            }
            return Ok(value);
        }
        let (
            Some(type_info),
            VariableState::Available {
                children: ValueChildren::Available(raw),
                presentation: None,
                ..
            },
        ) = (&value.type_info, &value.state)
        else {
            return Ok(value);
        };
        let Some(bound) = controller.view_choice(type_info.reference).bound.clone() else {
            return Ok(value);
        };
        let raw = Arc::clone(raw);
        let this = StopPlace {
            module: raw.module,
            located: Located {
                ty: raw.target_type,
                storage: raw.storage.clone(),
            },
        };
        let mut share = self.budget.share();
        let result = {
            let mut machine = StopMachine::new(self.frame, &mut share, self.interruptible);
            machine.depth = self.depth + 1;
            crate::view::run::present(&bound, &mut machine, this)
        };
        self.budget.absorb(share.usage());
        let name = crate::view::name_of(&bound.view);
        let (presentation, text) = match result {
            Ok(presented) => {
                let fields = bound.fields.len() as u64;
                let inner = presented
                    .inner
                    .as_ref()
                    .and_then(|inner| match &inner.state {
                        VariableState::Available {
                            presentation: Some(presentation),
                            ..
                        } if presentation.shape != PresentedShape::Raw => {
                            match &presentation.children {
                                ValueChildren::Available(reference) => Some(Arc::clone(reference)),
                                _ => None,
                            }
                        }
                        VariableState::Available {
                            children: ValueChildren::Available(reference),
                            ..
                        } => Some(Arc::clone(reference)),
                        _ => None,
                    });
                let elements = inner
                    .as_ref()
                    .map_or_else(|| presented.count.unwrap_or(0), |inner| inner.total());
                let mut reference = (*raw).clone();
                reference.total = elements.saturating_add(fields).saturating_add(1);
                reference.active_variant = None;
                reference.view = Some(ViewChildren {
                    bound: bound as Arc<dyn std::any::Any + Send + Sync>,
                    elements,
                    fields,
                    inner,
                });
                (
                    Presentation {
                        view: name,
                        shape: presented.shape,
                        count: presented.count.map(PresentedCount::Exact),
                        summary: presented.summary.into(),
                        children: ValueChildren::Available(Arc::new(reference)),
                        problem: presented.partial,
                    },
                    presented.text,
                )
            }
            Err(Failure::Problem(problem)) => (failed(name, problem), None),
            Err(Failure::Debugger(error)) => return Err(Stop::Failed(error)),
        };
        if let VariableState::Available {
            text: state_text,
            presentation: state_presentation,
            ..
        } = &mut value.state
        {
            if let Some(text) = text {
                *state_text = Some(Arc::new(text));
            }
            *state_presentation = Some(Arc::new(presentation));
        }
        Ok(value)
    }

    /// The text a view presents the value at `place` as, when it presents
    /// values of `ty` as text, or the presentation that says why it could
    /// not.
    fn pointee_text(
        &mut self,
        ty: TypeReference,
        place: StopPlace,
    ) -> std::result::Result<(Option<crate::TextSummary>, Option<Presentation>), Stop> {
        let Some(bound) = self.frame.controller.view_choice(ty).bound.clone() else {
            return Ok((None, None));
        };
        if !bound.shape.has_text() {
            return Ok((None, None));
        }
        let mut share = self.budget.share();
        let result = {
            let mut machine = StopMachine::new(self.frame, &mut share, self.interruptible);
            machine.depth = self.depth + 1;
            crate::view::run::present(&bound, &mut machine, place)
        };
        self.budget.absorb(share.usage());
        match result {
            Ok(presented) => Ok((presented.text, None)),
            Err(Failure::Problem(problem)) => Ok((
                None,
                Some(failed(crate::view::name_of(&bound.view), problem)),
            )),
            Err(Failure::Debugger(error)) => Err(Stop::Failed(error)),
        }
    }

    /// The place of element `index` of a value a view presents, for
    /// `v[i]`.
    pub(super) fn view_element(
        &mut self,
        bound: &Arc<ViewBound>,
        from: &StopPlace,
        index: i128,
    ) -> std::result::Result<StopPlace, Stop> {
        let mut machine = StopMachine::new(self.frame, self.budget, self.interruptible);
        machine.depth = self.depth;
        match crate::view::run::element_place(bound, &mut machine, from.clone(), index) {
            Ok(place) => Ok(place),
            Err(Failure::Debugger(error)) => Err(Stop::Failed(error)),
            Err(Failure::Problem(ViewProblem::Unavailable(reason))) => {
                Err(Stop::missing(VariableState::Unavailable(reason)))
            }
            Err(Failure::Problem(problem)) => Err(Stop::Refused(Refusal::new(
                crate::ExpressionErrorKind::Unsupported,
                format!("the view {}: {problem}", bound.view.header),
            ))),
        }
    }

    /// How many elements the value at `at` holds, or the length of its
    /// text, as the view of its type presents it, for `len(v)`.
    pub(super) fn view_length(&mut self, at: &StopPlace) -> std::result::Result<Option<u64>, Stop> {
        let controller = self.frame.controller;
        if !controller.views.enabled {
            return Ok(None);
        }
        let ty = TypeReference {
            image: self.module(at.module)?.loaded.image,
            id: at.located.ty,
        };
        let Some(bound) = controller.view_choice(ty).bound.clone() else {
            return Ok(None);
        };
        let mut machine = StopMachine::new(self.frame, self.budget, self.interruptible);
        machine.depth = self.depth;
        match crate::view::run::length(&bound, &mut machine, at.clone()) {
            Ok(length) => Ok(Some(length)),
            Err(Failure::Debugger(error)) => Err(Stop::Failed(error)),
            Err(Failure::Problem(ViewProblem::Unavailable(reason))) => {
                Err(Stop::missing(VariableState::Unavailable(reason)))
            }
            Err(Failure::Problem(problem)) => Err(Stop::Refused(Refusal::new(
                crate::ExpressionErrorKind::Type,
                format!("the view {}: {problem}", bound.view.header),
            ))),
        }
    }

    /// A value the provider read, with its presentation.
    pub(super) fn present_state(
        &mut self,
        type_info: Option<TypeInfo>,
        state: VariableState,
    ) -> Result<VariableState> {
        let value = self.finish(type_info, state);
        self.presented(value)
            .map(|value| value.state)
            .map_err(stopped)
    }
}

/// The presentation of a value a view failed to present, which shows it as
/// stored, with the reason.
fn failed(view: Arc<crate::ViewName>, problem: ViewProblem) -> Presentation {
    Presentation {
        view,
        shape: PresentedShape::Raw,
        count: None,
        summary: problem.to_string().into(),
        children: ValueChildren::NotApplicable,
        problem: Some(problem),
    }
}

use crate::eval::target::Machine as _;
