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
use crate::model::ValueStorage;
use crate::model::ViewChildren;
use crate::protocol::StopId;
use crate::view::bind::BoundShape;
use crate::view::run::{Child, Failure};
use crate::view::scan::Checkpoints;
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
use crate::TypeKind;

/// How deeply views present values inside the values they present.
const MAX_DEPTH: u8 = 4;

/// The most values whose scans the controller keeps checkpoints for at one
/// stop; the oldest is forgotten first.
const MAX_SCANS: usize = 64;

/// The views a controller presents values with, the view each type has,
/// and where the scans of values presented at this stop have been.
pub(super) struct Views {
    /// The files loaded for the session, tried before a module's own views
    /// and the built-in ones.
    pub(super) set: Arc<ViewSet>,
    pub(super) enabled: bool,
    /// The view each type has under this set, bound when first needed.
    choices: RefCell<BTreeMap<TypeReference, Arc<Choice<StopStep>>>>,
    scans: RefCell<Scans>,
}

/// Which value a scan presents: its type, its view, and its storage.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ScanKey {
    ty: TypeReference,
    view: (Arc<str>, u32),
    storage: ValueStorage,
}

/// The checkpoints of scans at one stop, oldest first. They hold while the
/// program's state does, so a new stop or a write forgets them.
#[derive(Default)]
struct Scans {
    stop: Option<StopId>,
    entries: Vec<(ScanKey, Checkpoints)>,
}

impl Default for Views {
    fn default() -> Self {
        Self {
            set: ViewSet::empty(),
            enabled: true,
            choices: RefCell::default(),
            scans: RefCell::default(),
        }
    }
}

impl Views {
    /// Presents with another set from now on.
    pub(super) fn replace(&mut self, set: Arc<ViewSet>) {
        self.set = set;
        self.choices.get_mut().clear();
        self.forget_scans();
    }

    /// Forgets where scans have been, after the program's state changed.
    pub(super) fn forget_scans(&self) {
        self.scans.borrow_mut().entries.clear();
    }

    /// The checkpoints of a value's scan at `stop`.
    fn checkpoints(&self, stop: StopId, key: &ScanKey) -> Checkpoints {
        let mut scans = self.scans.borrow_mut();
        if scans.stop != Some(stop) {
            scans.stop = Some(stop);
            scans.entries.clear();
        }
        scans
            .entries
            .iter()
            .find(|(existing, _)| existing == key)
            .map(|(_, checkpoints)| checkpoints.clone())
            .unwrap_or_default()
    }

    /// Keeps a value's checkpoints at `stop`.
    fn keep(&self, stop: StopId, key: ScanKey, checkpoints: Checkpoints) {
        let mut scans = self.scans.borrow_mut();
        if scans.stop != Some(stop) {
            return;
        }
        scans.entries.retain(|(existing, _)| *existing != key);
        if checkpoints == Checkpoints::default() {
            return;
        }
        if scans.entries.len() == MAX_SCANS {
            scans.entries.remove(0);
        }
        scans.entries.push((key, checkpoints));
    }
}

/// The key of the scan presenting `place` with `bound`.
fn scan_key(bound: &ViewBound, image: crate::ModuleImageId, place: &StopPlace) -> ScanKey {
    ScanKey {
        ty: TypeReference {
            image,
            id: place.located.ty,
        },
        view: (Arc::clone(&bound.view.source), bound.view.line),
        storage: place.located.storage.clone(),
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

    fn same_type(&self, left: TypeReference, right: TypeReference) -> bool {
        left.image == self.module.loaded.image && self.module.image.same_type(left, right)
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

    fn types_with_base(&self, base: &str) -> Vec<TypeReference> {
        self.module.image.types_with_base(base)
    }

    fn global_step(
        &self,
        name: &str,
    ) -> std::result::Result<Option<(StopStep, TypeReference)>, Refusal> {
        let global = match self.module.image.global_named(name) {
            Ok(global) => global,
            Err(Error::VariableNotFound(_)) => return Ok(None),
            Err(error) => {
                return Err(Refusal::new(
                    crate::eval::error::ErrorKind::AmbiguousName,
                    error.to_string(),
                ));
            }
        };
        let refuse = |error: Error| {
            Refusal::new(
                crate::eval::error::ErrorKind::Unsupported,
                error.to_string(),
            )
        };
        let key = self
            .module
            .variables
            .global_object(global.id)
            .map_err(refuse)?;
        let ty = self.module.variables.object_type(key).map_err(|reason| {
            Refusal::new(
                crate::eval::error::ErrorKind::Unsupported,
                format!("`{name}` has a malformed type: {reason}"),
            )
        })?;
        Ok(Some((
            StopStep::Global(super::evaluation::StopObject::global(
                self.module.loaded.id,
                key,
            )),
            TypeReference {
                image: self.module.loaded.image,
                id: ty,
            },
        )))
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
                    crate::view::choose_among(
                        &[&self.views.set, module.image.views(), &ViewSet::built_in()],
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

    /// The views whose patterns name the types `name` means in each module,
    /// and why each did not bind.
    pub(super) fn explain_type(&self, name: &str) -> Vec<crate::TypeViews> {
        let mut found = Vec::new();
        for module in self.modules.values() {
            let mut seen = Vec::<TypeReference>::new();
            for reference in module.image.types_named(name) {
                if seen
                    .iter()
                    .any(|other| module.image.same_type(*other, reference))
                {
                    continue;
                }
                seen.push(reference);
                found.extend(self.type_views(module, reference));
            }
        }
        found
    }

    /// How every module's types are presented: each type a view's pattern
    /// names, once however many units define it, and the session's and
    /// modules' own views that present no type.
    pub(super) fn check_views(&self) -> crate::ViewCheck {
        let built_in = ViewSet::built_in();
        let mut types = Vec::new();
        let mut used = std::collections::BTreeSet::new();
        for module in self.modules.values() {
            let sets = [&*self.views.set, &**module.image.views(), &*built_in];
            let mut seen = std::collections::BTreeSet::new();
            for node in module.image.types() {
                let crate::model::TypeNode::Resolved(info) = node else {
                    continue;
                };
                let Some(identity) = info.identity.as_deref() else {
                    continue;
                };
                if !sets.iter().any(|set| set.names(identity))
                    || module
                        .image
                        .type_key(info.reference)
                        .is_some_and(|key| !seen.insert(Arc::clone(key)))
                {
                    continue;
                }
                let Some(views) = self.type_views(module, info.reference) else {
                    continue;
                };
                if views.candidates.is_empty() {
                    continue;
                }
                used.extend(
                    views
                        .candidates
                        .iter()
                        .filter(|candidate| candidate.rejection.is_none())
                        .map(|candidate| (Arc::clone(&candidate.view.source), candidate.view.line)),
                );
                types.push(views);
            }
        }
        types.sort_by(|left, right| {
            (&left.type_info.name, &left.module).cmp(&(&right.type_info.name, &right.module))
        });
        let unused = self
            .views
            .set
            .views()
            .iter()
            .chain(
                self.modules
                    .values()
                    .flat_map(|module| module.image.views().views().iter()),
            )
            .filter(|view| !used.contains(&(Arc::clone(&view.source), view.line)))
            .map(|view| crate::view::name_of(view))
            .collect();
        crate::ViewCheck {
            types: types.into(),
            unused,
        }
    }

    /// The views whose patterns name one type, as its choice tried them.
    fn type_views(
        &self,
        module: &RuntimeModule,
        reference: TypeReference,
    ) -> Option<crate::TypeViews> {
        let type_info = module.image.type_info(reference)?.clone();
        let choice = self.view_choice(reference);
        Some(crate::TypeViews {
            type_info,
            module: Arc::from(module.image.path()),
            candidates: choice
                .candidates
                .iter()
                .map(|candidate| crate::ViewCandidate {
                    view: Arc::clone(&candidate.name),
                    rejection: candidate
                        .rejection
                        .as_ref()
                        .map(|rejection| rejection.to_string().into()),
                })
                .collect(),
        })
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
        let bound = view
            .bound
            .clone()
            .map(|bound| {
                bound.downcast::<ViewBound>().map_err(|_| {
                    Error::InvalidValueExpression("the view belongs to another debugger".into())
                })
            })
            .transpose()?;
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
        // What the debugger presents without a view has only `[raw]` after
        // the elements it lends.
        if bound.is_none() && completion_budget_exhausted.is_none() && next < end {
            let mut machine = StopMachine::new(&scope, budget, true);
            children.push(value_child(Child::Raw, &this, &mut machine)?);
        }
        if let (Some(bound), true) = (&bound, completion_budget_exhausted.is_none() && next < end) {
            let mut machine = StopMachine::new(&scope, budget, true);
            // With borrowed elements, the view's own children start at
            // its fields.
            let (start, elements) = if view.inner.is_some() {
                (next - view.elements, 0)
            } else {
                (next, view.elements)
            };
            let key = scan_key(bound, reference.image, &this);
            let mut checkpoints = self.views.checkpoints(reference.stop_id, &key);
            let presented = crate::view::run::children(
                bound,
                &mut machine,
                this.clone(),
                elements,
                start,
                end - next,
                &mut checkpoints,
            );
            self.views.keep(reference.stop_id, key, checkpoints);
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
                children.push(value_child(child, &this, &mut machine)?);
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

/// A child a view presents, as the public model has it.
fn value_child<P: InspectionOps>(
    child: Child,
    this: &StopPlace,
    machine: &mut StopMachine<'_, '_, P>,
) -> Result<ValueChild> {
    Ok(match child {
        Child::Element(index, value) => ValueChild {
            relationship: ValueChildRelationship::Element { index },
            type_info: value.type_info.unwrap_or_else(|| placeholder(this)),
            state: value.state,
        },
        Child::Entry(index, key, value) => ValueChild {
            relationship: ValueChildRelationship::Entry {
                index,
                key: Arc::new(crate::MapKey {
                    type_info: key.type_info.clone().unwrap_or_else(|| placeholder(this)),
                    state: key.state,
                }),
            },
            type_info: value.type_info.unwrap_or_else(|| placeholder(this)),
            state: value.state,
        },
        Child::Field(name, value) => ValueChild {
            relationship: ValueChildRelationship::Field { name },
            type_info: value.type_info.unwrap_or_else(|| placeholder(this)),
            state: value.state,
        },
        Child::Raw => {
            let module = machine.module(this.module).map_err(stopped)?;
            let value = machine
                .materialize(module, &this.located)
                .map_err(stopped)?;
            ValueChild {
                relationship: ValueChildRelationship::Raw,
                type_info: value.type_info.unwrap_or_else(|| placeholder(this)),
                state: value.state,
            }
        }
    })
}

/// What a value dynamically is.
enum Dynamic {
    /// Nothing, as a nil interface holds.
    Nil,
    /// A value of type `ty` at `place`.
    Value { ty: TypeReference, place: StopPlace },
}

/// A type and the types its typedefs and qualifiers stand for, outermost
/// first.
fn typedef_chain(types: &dyn TypeSource, ty: TypeReference) -> Vec<TypeInfo> {
    let mut chain = Vec::new();
    let mut current = Some(ty);
    while let Some(ty) = current.filter(|_| chain.len() < 64) {
        let Some(info) = types.type_info(ty) else {
            break;
        };
        current = match &info.kind {
            TypeKind::Modified { target, .. }
            | TypeKind::Named {
                target: Some(target),
                ..
            } => Some(*target),
            _ => None,
        };
        chain.push(info);
    }
    chain
}

/// Whether a C++ class is polymorphic: it, or a base at its start, holds a
/// vtable pointer, which GCC names `_vptr.X` and Clang `_vptr$X`.
fn polymorphic(types: &dyn TypeSource, ty: TypeReference, depth: usize) -> bool {
    let Ok((_, info)) = crate::eval::types::representation(types, ty) else {
        return false;
    };
    let TypeKind::Record { members, bases, .. } = &info.kind else {
        return false;
    };
    depth < 16
        && (members.iter().any(|member| {
            member.artificial
                && member
                    .name
                    .as_deref()
                    .is_some_and(|name| name.starts_with("_vptr"))
        }) || bases.iter().any(|base| {
            matches!(base.layout, crate::RecordMemberLayout::ByteOffset(0))
                && polymorphic(types, base.type_ref, depth + 1)
        }))
}

/// The offset and type of member `name` in the record a pointer of type
/// `pointer` points to.
fn pointee_member(
    types: &dyn TypeSource,
    pointer: TypeReference,
    name: &str,
) -> Option<(u64, TypeReference)> {
    let (_, info) = crate::eval::types::representation(types, pointer).ok()?;
    let TypeKind::Pointer {
        target: Some(target),
        ..
    } = info.kind
    else {
        return None;
    };
    let (_, record) = crate::eval::types::representation(types, target).ok()?;
    let TypeKind::Record { members, .. } = &record.kind else {
        return None;
    };
    let member = members
        .iter()
        .find(|member| member.name.as_deref() == Some(name))?;
    match member.layout {
        crate::RecordMemberLayout::ByteOffset(offset) => Some((offset, member.type_ref)),
        _ => None,
    }
}

/// The one type a name means in an image, as its several units' copies of
/// one type are one.
fn one_type(image: &crate::ModuleImage, name: &str) -> Option<TypeReference> {
    let found = image
        .types_named(name)
        .into_iter()
        .filter(|reference| {
            image
                .type_info(*reference)
                .is_some_and(|info| info.identity.is_some())
        })
        .collect::<Vec<_>>();
    let first = *found.first()?;
    found
        .iter()
        .all(|other| image.same_type(first, *other))
        .then_some(first)
}

/// The most children of a variant a sum's presentation reads.
const MAX_SUM_CHILDREN: u32 = 64;

/// How a sum type's value reads: the value standing for it, if any, and
/// its summary.
struct Sum {
    payload: Option<ValueChild>,
    summary: String,
}

impl Sum {
    /// The sum whose active variant is `name`, with `members`. A variant
    /// whose one member is named as it holds that member: Rust's is a record
    /// of the variant's fields, `__0` and on for a tuple variant, and a Zig
    /// tagged union's any value: `Some(42)`, `Ok(7)`, `Point {x: 1, y: 2}`,
    /// `circle(3)`, `None`. A Zig optional or error union is its payload,
    /// `null`, or its error.
    fn of<P: InspectionOps>(
        language: Option<crate::SourceLanguage>,
        name: &str,
        members: &[ValueChild],
        machine: &mut StopMachine<'_, '_, P>,
    ) -> std::result::Result<Self, Stop> {
        let zig = language == Some(crate::SourceLanguage::Zig);
        match (zig, name, members) {
            (true, "null", []) => {
                return Ok(Self {
                    payload: None,
                    summary: "null".to_owned(),
                });
            }
            (true, "some" | "success" | "error", [member]) => {
                return Ok(Self {
                    summary: crate::view::summary::value(Some(&member.type_info), &member.state),
                    payload: Some(member.clone()),
                });
            }
            _ => {}
        }
        // One member named as the variant is its payload: a Rust variant's
        // record of fields, or a Zig tagged union's value.
        let fields = match members {
            [member] if member_name(member) == Some(name) => match &member.state {
                VariableState::Available {
                    value: crate::VariableValue::Record,
                    children: ValueChildren::Available(reference),
                    presentation: None,
                    ..
                } => {
                    let reference = Arc::clone(reference);
                    machine.children_of(&reference)?
                }
                _ => {
                    return Ok(Self {
                        summary: format!(
                            "{name}({})",
                            crate::view::summary::value(Some(&member.type_info), &member.state)
                        ),
                        payload: Some(member.clone()),
                    });
                }
            },
            _ => members.to_vec(),
        };
        let tuple = fields
            .iter()
            .all(|field| member_name(field).is_some_and(|field| field.starts_with("__")));
        let rendered = fields
            .iter()
            .map(|field| crate::view::summary::value(Some(&field.type_info), &field.state))
            .collect::<Vec<_>>();
        let summary = match (fields.as_slice(), tuple) {
            ([], _) => name.to_owned(),
            (_, true) => format!("{name}({})", rendered.join(", ")),
            _ => format!(
                "{name} {{{}}}",
                fields
                    .iter()
                    .zip(&rendered)
                    .map(|(field, value)| format!(
                        "{}: {value}",
                        member_name(field).unwrap_or("<anonymous>")
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        // A variant of one field stands for that field; one of several, or
        // a Rust variant's record, for its members.
        let payload = match (fields.as_slice(), members) {
            ([field], _) => Some(field.clone()),
            ([], _) => None,
            (_, [member]) => Some(member.clone()),
            _ => None,
        };
        Ok(Self { payload, summary })
    }
}

/// A child's member name.
fn member_name(child: &ValueChild) -> Option<&str> {
    match &child.relationship {
        ValueChildRelationship::Member(member) => member.name.as_deref(),
        _ => None,
    }
}

/// The children a value lends the value it stands for, and how many: a
/// presented value's elements and fields, but not its `[raw]`, which is
/// its own; or the members of one no view presents.
fn lent(state: &VariableState) -> Option<(Arc<ValueChildrenReference>, u64)> {
    match state {
        VariableState::Available {
            presentation: Some(presentation),
            ..
        } if presentation.shape != PresentedShape::Raw => match &presentation.children {
            ValueChildren::Available(reference) => {
                Some((Arc::clone(reference), reference.total().saturating_sub(1)))
            }
            _ => None,
        },
        VariableState::Available {
            children: ValueChildren::Available(reference),
            ..
        } => Some((Arc::clone(reference), reference.total())),
        _ => None,
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

impl<'a, P: InspectionOps> StopMachine<'a, '_, P> {
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
        let viewed = value
            .type_info
            .as_ref()
            .is_some_and(|info| controller.view_choice(info.reference).bound.is_some());
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
            && !viewed
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
                presentation: None, ..
            },
        ) = (&value.type_info, &value.state)
        else {
            return Ok(value);
        };
        let Some(bound) = controller.view_choice(type_info.reference).bound.clone() else {
            return self.built_in(value);
        };
        // An aggregate's children say where it is; another value, such as
        // a Go map's pointer, is where its state was read from.
        let Some(raw) = self.place_of(type_info, &value.state) else {
            return Ok(value);
        };
        let this = StopPlace {
            module: raw.module,
            located: Located {
                ty: raw.target_type,
                storage: raw.storage.clone(),
            },
        };
        let key = scan_key(&bound, type_info.reference.image, &this);
        let mut checkpoints = controller.views.checkpoints(self.frame.stop_id, &key);
        let result = self
            .in_share(|machine| crate::view::run::present(&bound, machine, this, &mut checkpoints));
        controller.views.keep(self.frame.stop_id, key, checkpoints);
        let name = crate::view::name_of(&bound.view);
        let (presentation, text) = match result {
            Ok(presented) => {
                let fields = presented.named;
                let inner = presented
                    .inner
                    .as_ref()
                    .and_then(|inner| lent(&inner.state));
                // A value presented as a sequence or map stands for this one
                // as that sequence or map: its elements, then this value's
                // own fields and `[raw]`.
                let collection = presented
                    .inner
                    .as_ref()
                    .and_then(|inner| match &inner.state {
                        VariableState::Available {
                            presentation: Some(presentation),
                            ..
                        } if matches!(
                            presentation.shape,
                            PresentedShape::Sequence | PresentedShape::Map
                        ) =>
                        {
                            Some((presentation.shape, presentation.count))
                        }
                        _ => None,
                    });
                let (shape, count) = collection.unwrap_or((presented.shape, presented.count));
                let elements = match (&inner, collection) {
                    (Some(_), Some((_, count))) => count.map_or(0, PresentedCount::known),
                    (Some((_, lent)), None) => *lent,
                    (None, _) => presented.count.map_or(0, PresentedCount::known),
                };
                let inner = inner.map(|(reference, _)| reference);
                let mut reference = (*raw).clone();
                reference.total = elements.saturating_add(fields).saturating_add(1);
                reference.active_variant = None;
                reference.view = Some(ViewChildren {
                    bound: Some(bound as Arc<dyn std::any::Any + Send + Sync>),
                    elements,
                    fields,
                    inner,
                });
                (
                    Presentation {
                        view: name,
                        shape,
                        count,
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

    /// What the debugger presents of a value no view presents, from its
    /// debug information alone: a value as the type it dynamically is, or a
    /// sum type as its active variant.
    fn built_in(&mut self, value: InspectedValue) -> std::result::Result<InspectedValue, Stop> {
        if self.dynamic
            && let Some(dynamic) = self.dynamic(&value)?
        {
            return self.with_dynamic(value, dynamic);
        }
        let (
            Some(type_info),
            VariableState::Available {
                value:
                    crate::VariableValue::Variant {
                        active: Some(variant),
                        ..
                    },
                children: ValueChildren::Available(raw),
                presentation: None,
                ..
            },
        ) = (&value.type_info, &value.state)
        else {
            return Ok(value);
        };
        let (variant, raw) = (Arc::clone(variant), Arc::clone(raw));
        let language = type_info
            .identity
            .as_ref()
            .map(|identity| identity.language);
        let children = self.children_of(&raw)?;
        let first = children.len().saturating_sub(variant.members.len());
        let members = &children[first..];
        let name = variant
            .name
            .clone()
            .or_else(|| match variant.members.as_ref() {
                [member] => member.name.clone(),
                _ => None,
            })
            .unwrap_or_else(|| Arc::from("<unnamed variant>"));
        let sum = Sum::of(language, &name, members, self)?;
        Ok(Self::with_sum(value, &raw, sum))
    }

    /// What a value dynamically is, when its debug information and the
    /// program's own tables say so exactly: the object a C++ vtable
    /// pointer belongs to, the value a Rust trait object or Go interface
    /// holds, or nothing at all, as a nil interface holds.
    fn dynamic(&mut self, value: &InspectedValue) -> std::result::Result<Option<Dynamic>, Stop> {
        let Some(type_info) = &value.type_info else {
            return Ok(None);
        };
        let Some(this) = self.place_of(type_info, &value.state) else {
            return Ok(None);
        };
        let place = StopPlace {
            module: this.module,
            located: Located {
                ty: this.target_type,
                storage: this.storage.clone(),
            },
        };
        let language = typedef_chain(self, type_info.reference)
            .iter()
            .find_map(|info| info.identity.as_ref().map(|identity| identity.language));
        match language {
            Some(crate::SourceLanguage::Cpp) => self.cpp_dynamic(type_info, &place),
            Some(crate::SourceLanguage::Rust) => self.rust_dynamic(type_info, &place),
            Some(crate::SourceLanguage::Go) => self.go_dynamic(type_info, &place),
            _ => Ok(None),
        }
    }

    /// A pointer-sized word of memory.
    fn word(&mut self, address: u64) -> std::result::Result<u64, Stop> {
        let bytes = self.read(address, 8)?;
        Ok(u64::from_le_bytes(bytes.try_into().map_err(|_| {
            Stop::Refused(Refusal::new(
                crate::ExpressionErrorKind::Unsupported,
                "a short read",
            ))
        })?))
    }

    /// The module a virtual address lies in, with the address in its image.
    fn module_at(&self, address: u64) -> Option<(&'a RuntimeModule, crate::ImageAddress)> {
        let controller: &'a Controller<P> = self.frame.controller;
        controller.modules.values().find_map(|module| {
            let image = module
                .loaded
                .image_address(crate::VirtualAddress::new(address))
                .ok()?;
            module
                .image
                .contains_address(image)
                .then_some((module, image))
        })
    }

    /// A C++ object of a polymorphic class is the object its vtable pointer
    /// belongs to (the Itanium ABI): the pointer lies in the vtable group a
    /// `vtable for X` symbol names, the word before the pointer's target
    /// says how far the whole object begins before this one, and that
    /// object's own vtable pointer must be the group's primary one.
    fn cpp_dynamic(
        &mut self,
        type_info: &TypeInfo,
        place: &StopPlace,
    ) -> std::result::Result<Option<Dynamic>, Stop> {
        if !polymorphic(self, type_info.reference, 0) {
            return Ok(None);
        }
        let crate::model::ValueStorage::Memory(address) = place.located.storage else {
            return Ok(None);
        };
        let address = address.get();
        let vptr = self.word(address)?;
        let Some((module, image)) = self.module_at(vptr) else {
            return Ok(None);
        };
        let Some((class, group)) = module.image.vtable_class(image) else {
            return Ok(None);
        };
        let offset_to_top = self.word(vptr.wrapping_sub(16))?.cast_signed();
        let Some(whole) = address.checked_add_signed(offset_to_top) else {
            return Ok(None);
        };
        let primary = module
            .loaded
            .load_bias
            .wrapping_add(group.get())
            .wrapping_add(16);
        if self.word(whole)? != primary {
            return Ok(None);
        }
        let Some(ty) = one_type(&module.image, &class) else {
            return Ok(None);
        };
        if whole == address && module.image.same_type(ty, type_info.reference) {
            return Ok(None);
        }
        Ok(Some(Dynamic::Value {
            ty,
            place: StopPlace {
                module: module.loaded.id,
                located: Located {
                    ty: ty.id,
                    storage: crate::model::ValueStorage::Memory(crate::VirtualAddress::new(whole)),
                },
            },
        }))
    }

    /// A Rust trait object, `{pointer, vtable}` with a `dyn` pointee, holds
    /// the value of the type its vtable, `<C as Trait>::{vtable}`, is for.
    fn rust_dynamic(
        &mut self,
        type_info: &TypeInfo,
        place: &StopPlace,
    ) -> std::result::Result<Option<Dynamic>, Stop> {
        let TypeKind::Record { members, .. } = &type_info.kind else {
            return Ok(None);
        };
        let member = |name: &str| {
            members
                .iter()
                .find(|member| member.name.as_deref() == Some(name))
        };
        let (Some(pointer), Some(vtable)) = (member("pointer"), member("vtable")) else {
            return Ok(None);
        };
        let points_to_dyn = self
            .type_info(pointer.type_ref)
            .and_then(|info| match info.kind {
                TypeKind::Pointer {
                    target: Some(target),
                    ..
                } => self.type_info(target),
                _ => None,
            })
            .is_some_and(|target| target.name.starts_with("dyn "));
        let (
            true,
            crate::model::ValueStorage::Memory(address),
            crate::RecordMemberLayout::ByteOffset(pointer_offset),
            crate::RecordMemberLayout::ByteOffset(vtable_offset),
        ) = (
            points_to_dyn,
            &place.located.storage,
            pointer.layout,
            vtable.layout,
        )
        else {
            return Ok(None);
        };
        let address = address.get();
        let data = self.word(address + pointer_offset)?;
        let table = self.word(address + vtable_offset)?;
        let Some((module, image)) = self.module_at(table) else {
            return Ok(None);
        };
        let Some(ty) = module.image.trait_object_type(image) else {
            return Ok(None);
        };
        Ok(Some(Dynamic::Value {
            ty,
            place: StopPlace {
                module: module.loaded.id,
                located: Located {
                    ty: ty.id,
                    storage: crate::model::ValueStorage::Memory(crate::VirtualAddress::new(data)),
                },
            },
        }))
    }

    /// A Go interface holds the value of the type its runtime type
    /// describes: its `_type`, or its `tab`'s `Type`, whose offset from
    /// `runtime.types` a type's `DW_AT_go_runtime_type` gives. The value is
    /// the data word itself when its type is stored directly, which Go 1.26
    /// says in `TFlag` and earlier Go in `Kind_`, and otherwise what the
    /// word points to. A nil interface holds nothing.
    fn go_dynamic(
        &mut self,
        type_info: &TypeInfo,
        place: &StopPlace,
    ) -> std::result::Result<Option<Dynamic>, Stop> {
        // Go marks a type's kind on a typedef, which a same-named typedef
        // may stand over.
        let is_interface = typedef_chain(self, type_info.reference).iter().any(|info| {
            info.identity
                .as_ref()
                .and_then(|identity| identity.go)
                .is_some_and(|go| go.kind == crate::GoKind::Interface)
        });
        let Ok((_, record)) = crate::eval::types::representation(self, type_info.reference) else {
            return Ok(None);
        };
        let (true, TypeKind::Record { members, .. }, crate::model::ValueStorage::Memory(address)) =
            (is_interface, &record.kind, &place.located.storage)
        else {
            return Ok(None);
        };
        let address = address.get();
        let offset = |name: &str| {
            members
                .iter()
                .find(|member| member.name.as_deref() == Some(name))
                .and_then(|member| match member.layout {
                    crate::RecordMemberLayout::ByteOffset(offset) => {
                        Some((offset, member.type_ref))
                    }
                    _ => None,
                })
        };
        let Some((data_offset, _)) = offset("data") else {
            return Ok(None);
        };
        // The runtime type, and the pointer type its DWARF describes it by.
        let (descriptor, descriptor_type) = if let Some((type_offset, ty)) = offset("_type") {
            (self.word(address + type_offset)?, ty)
        } else if let Some((tab_offset, tab_type)) = offset("tab") {
            let Some((type_field, ty)) = pointee_member(self, tab_type, "Type") else {
                return Ok(None);
            };
            let tab = self.word(address + tab_offset)?;
            (
                if tab == 0 {
                    0
                } else {
                    self.word(tab + type_field)?
                },
                ty,
            )
        } else {
            return Ok(None);
        };
        if descriptor == 0 {
            return Ok(Some(Dynamic::Nil));
        }
        let Some((module, image)) = self.module_at(descriptor) else {
            return Ok(None);
        };
        let Ok(types) = module.image.symbol_named("runtime.types") else {
            return Ok(None);
        };
        let Some(runtime_offset) = image.get().checked_sub(types.address.get()) else {
            return Ok(None);
        };
        let Some(ty) = module.image.go_runtime_type(runtime_offset) else {
            return Ok(None);
        };
        let (Some((tflag, _)), Some((kind, _))) = (
            pointee_member(self, descriptor_type, "TFlag"),
            pointee_member(self, descriptor_type, "Kind_"),
        ) else {
            return Ok(None);
        };
        let direct = self.read(descriptor + tflag, 1)?[0] & 0x20 != 0
            || self.read(descriptor + kind, 1)?[0] & 0x20 != 0;
        let data = address + data_offset;
        let storage = if direct { data } else { self.word(data)? };
        Ok(Some(Dynamic::Value {
            ty,
            place: StopPlace {
                module: module.loaded.id,
                located: Located {
                    ty: ty.id,
                    storage: crate::model::ValueStorage::Memory(crate::VirtualAddress::new(
                        storage,
                    )),
                },
            },
        }))
    }

    /// `value` with the presentation of what it dynamically is.
    fn with_dynamic(
        &mut self,
        mut value: InspectedValue,
        dynamic: Dynamic,
    ) -> std::result::Result<InspectedValue, Stop> {
        let Some(type_info) = value.type_info.clone() else {
            return Ok(value);
        };
        let Some(raw) = self.place_of(&type_info, &value.state) else {
            return Ok(value);
        };
        let (shape, summary, inner) = match dynamic {
            Dynamic::Nil => (PresentedShape::Empty, "nil".to_owned(), None),
            Dynamic::Value { ty, place } => {
                let module = self.module(place.module)?;
                let stored = self.materialize(module, &place.located)?;
                let mut machine = StopMachine::new(self.frame, self.budget, self.interruptible);
                machine.depth = self.depth + 1;
                let presented = machine.presented(stored)?;
                let name = self
                    .type_info(ty)
                    .map_or_else(|| "<unknown type>".to_owned(), |info| info.name.to_string());
                let go = self
                    .type_info(ty)
                    .and_then(|info| info.identity)
                    .is_some_and(|identity| identity.language == crate::SourceLanguage::Go);
                let pointee = if go {
                    self.pointee_summary(&presented.state)?
                } else {
                    None
                };
                let shown = match pointee {
                    Some(pointee) => pointee,
                    None => match self.record_summary(&presented.state, 0)? {
                        Some(fields) => fields,
                        None => crate::view::summary::value(
                            presented.type_info.as_ref(),
                            &presented.state,
                        ),
                    },
                };
                let summary = format!("{name} {shown}");
                (PresentedShape::Dynamic, summary, lent(&presented.state))
            }
        };
        let elements = inner.as_ref().map_or(0, |(_, lent)| *lent);
        let inner = inner.map(|(reference, _)| reference);
        let mut reference = (*raw).clone();
        reference.total = elements.saturating_add(1);
        reference.active_variant = None;
        reference.view = Some(ViewChildren {
            bound: None,
            elements,
            fields: 0,
            inner,
        });
        let presentation = Presentation {
            view: Arc::new(crate::ViewName {
                source: "uscope".into(),
                line: 0,
                header: "dynamic types".into(),
                extend: false,
            }),
            shape,
            count: None,
            summary: summary.into(),
            children: ValueChildren::Available(Arc::new(reference)),
            problem: None,
        };
        if let VariableState::Available {
            presentation: state_presentation,
            ..
        } = &mut value.state
        {
            *state_presentation = Some(Arc::new(presentation));
        }
        Ok(value)
    }

    /// A record's members on one line, `{x: 1, y: 2}`, with its base
    /// classes' members first, as many as fit a summary; `None` for what is
    /// no record or a view presents.
    fn record_summary(
        &mut self,
        state: &VariableState,
        depth: usize,
    ) -> std::result::Result<Option<String>, Stop> {
        let VariableState::Available {
            value: crate::VariableValue::Record,
            children: ValueChildren::Available(reference),
            presentation: None,
            text: None,
            ..
        } = state
        else {
            return Ok(None);
        };
        let reference = Arc::clone(reference);
        let mut parts = Vec::new();
        self.record_members(&reference, depth, &mut parts)?;
        let mut line = String::from("{");
        for (index, part) in parts.iter().enumerate() {
            if line.chars().count() > crate::view::summary::MAX_CHARACTERS {
                line.push_str(", …");
                break;
            }
            if index > 0 {
                line.push_str(", ");
            }
            line.push_str(part);
        }
        line.push('}');
        Ok(Some(line))
    }

    /// A pointer to a record as Go's debuggers show one an interface holds,
    /// `*{name: value, …}`, or `nil`; `None` for any other value.
    fn pointee_summary(
        &mut self,
        state: &VariableState,
    ) -> std::result::Result<Option<String>, Stop> {
        let VariableState::Available {
            value: crate::VariableValue::Address(address),
            dereference:
                crate::DereferenceState::Available(crate::DereferenceReference {
                    module,
                    target_type,
                    target: crate::model::DereferenceTarget::Address(target),
                    ..
                }),
            ..
        } = state
        else {
            return Ok(None);
        };
        if address.address.get() == 0 {
            return Ok(Some("nil".to_owned()));
        }
        let place = Located {
            ty: *target_type,
            storage: crate::model::ValueStorage::Memory(*target),
        };
        let module = self.module(*module)?;
        let pointee = self.materialize(module, &place)?;
        Ok(self
            .record_summary(&pointee.state, 0)?
            .map(|fields| format!("*{fields}")))
    }

    /// The `name: value` parts of a record's members, its bases' first.
    fn record_members(
        &mut self,
        reference: &ValueChildrenReference,
        depth: usize,
        parts: &mut Vec<String>,
    ) -> std::result::Result<(), Stop> {
        if depth > 4 || parts.len() >= crate::view::summary::MAX_ELEMENTS {
            return Ok(());
        }
        for child in self.children_of(reference)? {
            match &child.relationship {
                ValueChildRelationship::Base(_) => {
                    if let VariableState::Available {
                        children: ValueChildren::Available(base),
                        ..
                    } = &child.state
                    {
                        let base = Arc::clone(base);
                        self.record_members(&base, depth + 1, parts)?;
                    }
                }
                ValueChildRelationship::Member(member) if !member.artificial => {
                    parts.push(format!(
                        "{}: {}",
                        member.name.as_deref().unwrap_or("<anonymous>"),
                        crate::view::summary::value(Some(&child.type_info), &child.state)
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// A value's children as stored, each presented one level deeper.
    fn children_of(
        &mut self,
        reference: &ValueChildrenReference,
    ) -> std::result::Result<Vec<ValueChild>, Stop> {
        if reference.total == 0 {
            return Ok(Vec::new());
        }
        let module = self.module(reference.module)?;
        let mut runtime = self.frame.controller.frame_runtime(
            self.frame.inferior,
            self.frame.pid,
            self.frame.resolved,
            module,
        );
        let limit = u32::try_from(reference.total.min(u64::from(MAX_SUM_CHILDREN)))
            .unwrap_or(MAX_SUM_CHILDREN);
        let page = module
            .variables
            .value_children(reference, 0, limit, &mut runtime, self.budget)
            .map_err(Stop::Failed)?;
        let mut machine = StopMachine::new(self.frame, self.budget, self.interruptible);
        machine.depth = self.depth + 1;
        let mut children = page.children.to_vec();
        for child in &mut children {
            let state = std::mem::replace(
                &mut child.state,
                VariableState::Unavailable(crate::VariableUnavailableReason::EvaluationLimit),
            );
            machine.dynamic = !matches!(child.relationship, ValueChildRelationship::Base(_));
            let presented =
                machine.presented(machine.finish(Some(child.type_info.clone()), state))?;
            child.state = presented.state;
        }
        Ok(children)
    }

    /// `value` with the presentation of the sum it is.
    fn with_sum(
        mut value: InspectedValue,
        raw: &ValueChildrenReference,
        sum: Sum,
    ) -> InspectedValue {
        let inner = sum
            .payload
            .as_ref()
            .and_then(|payload| lent(&payload.state));
        let elements = inner.as_ref().map_or(0, |(_, lent)| *lent);
        let inner = inner.map(|(reference, _)| reference);
        let mut reference = raw.clone();
        reference.total = elements.saturating_add(1);
        reference.active_variant = None;
        reference.view = Some(ViewChildren {
            bound: None,
            elements,
            fields: 0,
            inner,
        });
        let presentation = Presentation {
            view: Arc::new(crate::ViewName {
                source: "uscope".into(),
                line: 0,
                header: "sum types".into(),
                extend: false,
            }),
            shape: if sum.payload.is_some() {
                PresentedShape::Value
            } else {
                PresentedShape::Empty
            },
            count: None,
            summary: sum.summary.into(),
            children: ValueChildren::Available(Arc::new(reference)),
            problem: None,
        };
        if let VariableState::Available {
            presentation: state_presentation,
            ..
        } = &mut value.state
        {
            *state_presentation = Some(Arc::new(presentation));
        }
        value
    }

    /// Runs a view one level deeper. A view presenting a value at the top
    /// runs on a share of the budget, so running out never takes the rest
    /// of the inspection with it; the values inside it share that share.
    fn in_share<T>(&mut self, run: impl FnOnce(&mut StopMachine<'_, '_, P>) -> T) -> T {
        if self.depth > 0 {
            let mut machine = StopMachine::new(self.frame, self.budget, self.interruptible);
            machine.depth = self.depth + 1;
            return run(&mut machine);
        }
        let mut share = self.budget.share();
        let result = {
            let mut machine = StopMachine::new(self.frame, &mut share, self.interruptible);
            machine.depth = self.depth + 1;
            run(&mut machine)
        };
        self.budget.absorb(share.usage());
        result
    }

    /// Where a value of `type_info` is, as a capability over its storage.
    fn place_of(
        &self,
        type_info: &TypeInfo,
        state: &VariableState,
    ) -> Option<Arc<ValueChildrenReference>> {
        let VariableState::Available {
            source,
            raw,
            children,
            ..
        } = state
        else {
            return None;
        };
        if let ValueChildren::Available(reference) = children {
            return Some(Arc::clone(reference));
        }
        let storage = match (source, raw) {
            (crate::VariableValueSource::Memory(address), _) => ValueStorage::Memory(*address),
            (source, Some(raw)) => ValueStorage::Bytes {
                source: source.clone(),
                raw: Arc::clone(raw),
                start: 0,
                end: raw.len(),
                address: None,
            },
            _ => return None,
        };
        let module = self.frame.module_of(type_info.reference)?;
        let context = self.context(module);
        Some(Arc::new(ValueChildrenReference {
            stop_id: context.stop_id,
            thread: context.thread,
            frame: context.frame,
            module: context.module,
            image: context.image,
            context_address: context.address,
            target_type: type_info.reference.id,
            storage,
            total: 0,
            active_variant: None,
            view: None,
        }))
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
        // A text view scans nothing.
        let result = self.in_share(|machine| {
            crate::view::run::present(&bound, machine, place, &mut Checkpoints::default())
        });
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
        let controller = self.frame.controller;
        let image = self.module(from.module)?.loaded.image;
        let key = scan_key(bound, image, from);
        let mut checkpoints = controller.views.checkpoints(self.frame.stop_id, &key);
        let mut machine = StopMachine::new(self.frame, self.budget, self.interruptible);
        machine.depth = self.depth;
        let result = crate::view::run::element_place(
            bound,
            &mut machine,
            from.clone(),
            index,
            &mut checkpoints,
        );
        controller.views.keep(self.frame.stop_id, key, checkpoints);
        match result {
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
        let key = scan_key(&bound, ty.image, at);
        let mut checkpoints = controller.views.checkpoints(self.frame.stop_id, &key);
        let mut machine = StopMachine::new(self.frame, self.budget, self.interruptible);
        machine.depth = self.depth;
        let result = crate::view::run::length(&bound, &mut machine, at.clone(), &mut checkpoints);
        controller.views.keep(self.frame.stop_id, key, checkpoints);
        match result {
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
