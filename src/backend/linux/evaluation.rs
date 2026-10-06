//! Expressions at a stop: a frame's names for binding, and its state for
//! running, over the debug-info providers' structural primitives.

use std::cell::{Cell, OnceCell};
use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use nix::unistd::Pid;

use crate::debug_info::{Located, ObjectKey, PlannedStep, Step};
use crate::eval::Evaluation;
use crate::eval::bind::{Mode, bind};
use crate::eval::error::ErrorKind;
use crate::eval::interp::{Failure, Outcome, run};
use crate::eval::number::Exact;
use crate::eval::syntax::ast::Tag;
use crate::eval::syntax::{Expression, Span};
use crate::eval::target::{
    Lookup, Machine, Planned, Refusal, Register, Scope, StepKind, Stop, TypeLookup, TypeQuery,
};
use crate::eval::types::{TypeSource, type_info};
use crate::inspection::InspectionBudget;
use crate::model::{DereferenceTarget, ValueStorage};
use crate::protocol::StopId;
use crate::{
    AddressValue, ByteOrder, CodeInstanceId, DereferenceReference, DereferenceState,
    DereferenceUnavailableReason, Error, ImageAddress, InspectedValue, ModuleId, RecordKind,
    RegisterSnapshot, Result, StackFrameId, TextSummary, TypeInfo, TypeKind, TypeNode,
    TypeReference, ValueChildren, VariableState, VariableUnavailableReason, VariableValue,
    VariableValueSource, VirtualAddress,
};

use super::frames::{FrameRegisters, ResolvedFrame};
use super::inspection::{
    LinuxVariableRuntime, global_context_address, validate_inspection_limits, variable_context,
};
use super::native::{InspectionOps, LinuxTraceOps};
use super::registers::x86_64_register_snapshot;
use super::{Controller, Inferior, RuntimeModule, validate_image_current};

/// A data object a frame names.
#[derive(Debug, Clone, Copy)]
pub(super) struct StopObject {
    module: ModuleId,
    key: ObjectKey,
    /// Whether it is a local or parameter of the frame.
    local: bool,
}

impl StopObject {
    /// A global of a module.
    pub(super) const fn global(module: ModuleId, key: ObjectKey) -> Self {
        Self {
            module,
            key,
            local: false,
        }
    }
}

/// A structural step planned from types alone.
#[derive(Clone)]
pub(super) enum StopStep {
    /// A step one module's image planned.
    Provider { module: ModuleId, step: PlannedStep },
    /// To an element of a value a view presents as a sequence.
    Element(Arc<ViewBound>),
    /// To a global, from anywhere: a view's `global(NAME)`.
    Global(StopObject),
}

/// A view bound against one type, whose steps are a stop's.
pub(super) type ViewBound = crate::view::bind::BoundView<StopStep>;

impl fmt::Debug for StopStep {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Provider { module, .. } => write!(formatter, "StopStep({module:?})"),
            Self::Element(bound) => write!(formatter, "StopStep(element of {})", bound.view.header),
            Self::Global(object) => write!(formatter, "StopStep(global {object:?})"),
        }
    }
}

/// Where a value of one module's types is.
#[derive(Debug, Clone)]
pub(super) struct StopPlace {
    pub(super) module: ModuleId,
    pub(super) located: Located,
}

/// One frame of a stop, as expressions see it.
pub(super) struct Frame<'a, P: InspectionOps> {
    pub(super) controller: &'a Controller<P>,
    inferior: &'a Inferior,
    pid: Pid,
    pub(super) stop_id: StopId,
    resolved: &'a ResolvedFrame,
    /// The frame's module, address, and inline instance, when it has debug
    /// information.
    code: Option<(&'a RuntimeModule, ImageAddress, Option<CodeInstanceId>)>,
    registers: OnceCell<Option<RegisterSnapshot>>,
    /// Units of work every machine running at the frame has done; the
    /// views presenting values one inside another each have a machine.
    charges: Cell<u32>,
}

impl<P: InspectionOps> Controller<P> {
    /// The loaded module whose image defines `ty`.
    pub(super) fn module_of(&self, ty: TypeReference) -> Option<&RuntimeModule> {
        self.modules
            .values()
            .find(|module| module.loaded.image == ty.image)
    }

    pub(super) fn pointer_size(&self) -> u8 {
        self.module_image.target().pointer_width.bytes()
    }
}

impl<'a, P: InspectionOps> Frame<'a, P> {
    pub(super) fn module(&self, id: ModuleId) -> Option<&'a RuntimeModule> {
        self.controller.modules.get(&id)
    }

    /// Reads the frame's registers and memory for values `module` describes.
    pub(super) fn runtime(&self, module: &'a RuntimeModule) -> LinuxVariableRuntime<'a, P> {
        self.controller
            .frame_runtime(self.inferior, self.pid, self.resolved, module)
    }

    /// Modules in the order names are looked up: the frame's first.
    fn modules(&self) -> impl Iterator<Item = &'a RuntimeModule> + 'a {
        let first = self.code.map(|(module, ..)| module);
        let id = first.map(|module| module.loaded.id);
        first.into_iter().chain(
            self.controller
                .modules
                .values()
                .filter(move |module| Some(module.loaded.id) != id),
        )
    }

    fn registers(&self) -> Option<&RegisterSnapshot> {
        self.registers
            .get_or_init(|| {
                let native = self.controller.ptrace.registers(self.pid).ok()?;
                let caller = match &self.resolved.registers {
                    FrameRegisters::Caller(registers) => Some(registers),
                    FrameRegisters::Thread(_) => None,
                };
                Some(x86_64_register_snapshot(
                    self.controller.revision,
                    self.pid,
                    self.controller.module_image.target(),
                    &native,
                    caller,
                ))
            })
            .as_ref()
    }
}

impl<P: InspectionOps> Frame<'_, P> {
    /// The global `name` names in exactly one loaded module.
    fn lookup_global(
        &self,
        name: &str,
    ) -> std::result::Result<Option<Lookup<StopObject>>, Refusal> {
        let mut globals = Vec::new();
        let mut candidates = BTreeSet::new();
        for module in self.modules() {
            match module.image.global_named(name) {
                Ok(global) => globals.push((module, global.id)),
                Err(Error::VariableNotFound(_)) => {}
                Err(Error::AmbiguousGlobalVariable {
                    candidates: found, ..
                }) => {
                    for candidate in found {
                        let file = candidate
                            .declaration_path
                            .as_ref()
                            .and_then(|path| path.file_name())
                            .map(|file| file.to_string_lossy().into_owned());
                        candidates.insert(file.map_or_else(
                            || candidate.qualified_name.to_string(),
                            |file| format!("{file}::{}", candidate.qualified_name),
                        ));
                    }
                }
                Err(error) => return Err(refusal(&error)),
            }
        }
        match (globals.as_slice(), candidates.is_empty()) {
            ([(module, id)], true) => {
                let key = module
                    .variables
                    .global_object(*id)
                    .map_err(|error| refusal(&error))?;
                Ok(Some(object(module, key, false)))
            }
            ([], true) => Ok(None),
            _ => {
                for (module, id) in &globals {
                    if let Some(global) = module.image.global(*id) {
                        let file = module
                            .image
                            .path()
                            .file_name()
                            .map(|file| file.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        candidates.insert(format!("{file}::{}", global.qualified_name));
                    }
                }
                Ok(Some(Lookup::Ambiguous(candidates.into_iter().collect())))
            }
        }
    }

    /// The enumerator `name` names, by its own name or qualified by its
    /// enumeration's, in the frame's module before others.
    fn lookup_enumerator(&self, name: &str) -> Option<Lookup<StopObject>> {
        for module in self.modules() {
            let mut found = Vec::new();
            for node in module.image.types() {
                let TypeNode::Resolved(info) = node else {
                    continue;
                };
                let TypeKind::Enumeration { enumerators, .. } = &info.kind else {
                    continue;
                };
                for enumerator in enumerators.iter() {
                    let qualified = format!("{}::{}", info.name, enumerator.name);
                    if enumerator.name.as_ref() == name || qualified == name {
                        found.push((qualified, Exact::from(enumerator.value), info.reference));
                    }
                }
            }
            found.sort_by(|left, right| left.0.cmp(&right.0));
            found.dedup_by(|left, right| left.0 == right.0 && left.1 == right.1);
            match found.as_slice() {
                [] => {}
                [(_, value, ty)] => {
                    return Some(Lookup::Enumerator {
                        value: *value,
                        ty: *ty,
                    });
                }
                _ => {
                    return Some(Lookup::Ambiguous(
                        found.into_iter().map(|(qualified, ..)| qualified).collect(),
                    ));
                }
            }
        }
        None
    }
}

/// A data object of `module`, as a name lookup finds it.
fn object(module: &RuntimeModule, key: ObjectKey, local: bool) -> Lookup<StopObject> {
    Lookup::Object {
        object: StopObject {
            module: module.loaded.id,
            key,
            local,
        },
        ty: module.variables.object_type(key).map(|id| TypeReference {
            image: module.loaded.image,
            id,
        }),
    }
}

impl<P: InspectionOps> TypeSource for Frame<'_, P> {
    fn type_info(&self, ty: TypeReference) -> Option<TypeInfo> {
        self.controller.module_of(ty)?.image.type_info(ty).cloned()
    }

    fn pointer_size(&self) -> u8 {
        self.controller.pointer_size()
    }

    fn byte_order(&self) -> ByteOrder {
        self.controller.module_image.target().byte_order
    }
}

/// A shallow description of a type's definition, under which two types of
/// one name are one type.
fn definition(image: &crate::ModuleImage, info: &TypeInfo) -> String {
    let name = |reference: TypeReference| {
        image
            .type_info(reference)
            .map_or_else(|| "?".to_owned(), |info| info.name.to_string())
    };
    let shape = match &info.kind {
        TypeKind::Record { members, .. } | TypeKind::Union { members, .. } => members
            .iter()
            .map(|member| {
                format!(
                    "{}:{:?}:{}",
                    member.name.as_deref().unwrap_or_default(),
                    member.layout,
                    name(member.type_ref)
                )
            })
            .collect::<Vec<_>>()
            .join(","),
        TypeKind::Named {
            target: Some(target),
            ..
        }
        | TypeKind::Modified { target, .. } => name(*target),
        TypeKind::Enumeration { enumerators, .. } => enumerators
            .iter()
            .map(|enumerator| format!("{}={:?}", enumerator.name, enumerator.value))
            .collect::<Vec<_>>()
            .join(","),
        TypeKind::Base(base) => format!("{:?}{:?}", base.encoding, base.bit_size),
        other => format!("{other:?}"),
    };
    format!("{}/{:?}/{shape}", info.name, info.byte_size)
}

const fn tag_matches(tag: Option<Tag>, kind: &TypeKind) -> bool {
    match tag {
        None => !matches!(
            kind,
            TypeKind::Pointer { .. } | TypeKind::Reference { .. } | TypeKind::Array { .. }
        ),
        Some(Tag::Struct | Tag::Class) => {
            matches!(
                kind,
                TypeKind::Record {
                    kind: RecordKind::Struct | RecordKind::Class,
                    ..
                }
            )
        }
        Some(Tag::Union) => matches!(kind, TypeKind::Union { .. }),
        Some(Tag::Enum) => matches!(kind, TypeKind::Enumeration { .. }),
    }
}

/// A provider's refusal of a step, as an expression error.
fn refusal(error: &Error) -> Refusal {
    let kind = match error {
        Error::AmbiguousBase { .. } => ErrorKind::AmbiguousName,
        Error::MemberNotFound { .. }
        | Error::BaseNotFound { .. }
        | Error::AmbiguousMember { .. }
        | Error::MemberAccessOnNonRecord { .. }
        | Error::IndexAccessOnNonIndexable { .. }
        | Error::IncompleteArrayIndex { .. }
        | Error::InvalidValueExpression(_) => ErrorKind::Type,
        Error::ValueIndexOutOfBounds { .. } => ErrorKind::Bounds,
        _ => ErrorKind::Unsupported,
    };
    Refusal::new(kind, error.to_string())
}

/// The loaded module that defines `ty`.
fn type_module<P: InspectionOps>(
    controller: &Controller<P>,
    ty: TypeReference,
) -> std::result::Result<&RuntimeModule, Refusal> {
    controller
        .module_of(ty)
        .ok_or_else(|| Refusal::new(ErrorKind::Unsupported, "the type's module is not loaded"))
}

/// Plans one step from a value of `from` in the image of its module.
pub(super) fn plan_in<P: InspectionOps>(
    controller: &Controller<P>,
    from: TypeReference,
    step: StepKind<'_>,
) -> std::result::Result<Planned<StopStep>, Refusal> {
    let module = type_module(controller, from)?;
    let image = &module.image;
    let base_name;
    let is_target;
    let step = match step {
        StepKind::Deref => Step::Deref,
        StepKind::Member(name) => Step::Member(name),
        StepKind::Index { available } => Step::Index { available },
        StepKind::Base(target) => {
            if target.image != from.image {
                return Err(Refusal::new(
                    ErrorKind::Type,
                    "a base class is in the same module as the class",
                ));
            }
            base_name = image
                .type_info(target)
                .map_or_else(|| Arc::from("?"), |info| Arc::clone(&info.name));
            is_target = move |id| {
                image.same_type(
                    TypeReference {
                        image: target.image,
                        id,
                    },
                    target,
                )
            };
            Step::Base(crate::debug_info::BaseTarget {
                name: &base_name,
                is_target: &is_target,
            })
        }
    };
    let planned = module
        .variables
        .plan_step(from.id, step)
        .map_err(|error| refusal(&error))?;
    Ok(Planned {
        result: planned.result().map(|id| TypeReference {
            image: module.loaded.image,
            id,
        }),
        consumed: planned.consumed(),
        step: StopStep::Provider {
            module: module.loaded.id,
            step: planned,
        },
    })
}

/// The types a name means in one module, through its image's identity
/// index: a name may omit outer path segments and trailing arguments. One
/// type defined alike in several units is one type, and so is a synonym
/// that only renames another candidate, as Go's typedefs of its named
/// types do.
pub(super) fn lookup_type_in(module: &RuntimeModule, query: &TypeQuery) -> TypeLookup {
    let candidates = module
        .image
        .types_named(&query.name)
        .into_iter()
        .filter_map(|reference| module.image.type_info(reference))
        .filter(|info| tag_matches(query.tag, &info.kind))
        .collect::<Vec<_>>();
    let mut found: Vec<(String, TypeReference)> = candidates
        .iter()
        .filter(|info| {
            !matches!(
                info.kind,
                TypeKind::Named { target: Some(target), .. }
                    if candidates.iter().any(|other| {
                        other.reference == target && other.name == info.name
                    })
            )
        })
        .map(|info| (definition(&module.image, info), info.reference))
        .collect();
    found.sort_by(|left, right| left.0.cmp(&right.0));
    found.dedup_by(|left, right| left.0 == right.0);
    match found.as_slice() {
        [] => TypeLookup::NotFound,
        [(_, ty)] => TypeLookup::Found(*ty),
        _ => TypeLookup::Ambiguous(
            found
                .into_iter()
                .map(|(definition, _)| definition)
                .collect(),
        ),
    }
}

impl<P: InspectionOps> Scope for Frame<'_, P> {
    type Object = StopObject;
    type Step = StopStep;

    fn lookup(
        &self,
        name: &str,
        outermost: bool,
    ) -> std::result::Result<Lookup<StopObject>, Refusal> {
        if !outermost && let Some((module, address, selected)) = self.code {
            match module.variables.visible_object(address, selected, name) {
                Ok(key) => return Ok(object(module, key, true)),
                Err(Error::VariableNotFound(_)) => {}
                Err(Error::AmbiguousVariable(_)) => {
                    return Ok(Lookup::Ambiguous(vec![name.to_owned()]));
                }
                Err(error) => return Err(refusal(&error)),
            }
        }
        if let Some(global) = self.lookup_global(name)? {
            return Ok(global);
        }
        Ok(self.lookup_enumerator(name).unwrap_or(Lookup::NotFound))
    }

    /// The types a name means, in the frame's module first.
    fn lookup_type(&self, query: &TypeQuery) -> TypeLookup {
        for module in self.modules() {
            match lookup_type_in(module, query) {
                TypeLookup::NotFound => {}
                found => return found,
            }
        }
        TypeLookup::NotFound
    }

    fn has_view(&self, ty: TypeReference) -> bool {
        self.controller.views.enabled
            && self
                .controller
                .view_choice(ty)
                .bound
                .as_ref()
                .is_some_and(|bound| bound.shape.has_elements() || bound.shape.has_text())
    }

    fn stands_for_container(&self, ty: TypeReference) -> bool {
        crate::view::stands_for_container(self, ty)
    }

    fn plan(
        &self,
        from: TypeReference,
        step: StepKind<'_>,
    ) -> std::result::Result<Planned<StopStep>, Refusal> {
        let planned = plan_in(self.controller, from, step);
        // A value with no indexing of its own is indexed through the view
        // that presents it as a sequence.
        match (&planned, step) {
            (Err(_), StepKind::Index { .. }) => {
                self.controller.view_index(from).map_or(planned, Ok)
            }
            _ => planned,
        }
    }

    fn register(&self, name: &str) -> Option<Register> {
        let name = match name {
            "pc" => "rip",
            "sp" => "rsp",
            "fp" => "rbp",
            other => other,
        };
        let registers = self.registers()?;
        let number = registers
            .registers
            .iter()
            .position(|value| value.register.name.as_ref() == name)?;
        let width = u8::try_from(registers.registers[number].register.bits.min(128)).ok()?;
        Some(Register {
            number: u16::try_from(number).ok()?,
            width,
        })
    }
}

/// A frame's state at one stop, which a bound program runs against.
pub(super) struct StopMachine<'a, 'b, P: InspectionOps> {
    pub(super) frame: &'b Frame<'a, P>,
    pub(super) budget: &'b mut InspectionBudget,
    /// How many views are presenting the values this machine presents, one
    /// inside another.
    pub(super) depth: u8,
    /// Whether a run-control request waiting ends the work, which then
    /// runs again after it; false for work run control itself does.
    pub(super) interruptible: bool,
    /// Whether values present as the types they dynamically are; false for
    /// a base-class subobject, which is part of an object, not one.
    pub(super) dynamic: bool,
}

/// How many units of work a frame's machines do between checks for waiting
/// run control.
pub(super) const INTERRUPT_INTERVAL: u32 = 64;

impl<'a, 'b, P: InspectionOps> StopMachine<'a, 'b, P> {
    pub(super) const fn new(
        frame: &'b Frame<'a, P>,
        budget: &'b mut InspectionBudget,
        interruptible: bool,
    ) -> Self {
        Self {
            frame,
            budget,
            depth: 0,
            interruptible,
            dynamic: true,
        }
    }

    /// A machine on this one's budget, presenting values `depth` views deep.
    pub(super) const fn nested(&mut self, depth: u8) -> StopMachine<'a, '_, P> {
        let mut machine = StopMachine::new(self.frame, self.budget, self.interruptible);
        machine.depth = depth;
        machine
    }

    pub(super) fn module(&self, id: ModuleId) -> std::result::Result<&'a RuntimeModule, Stop> {
        self.frame
            .module(id)
            .ok_or_else(|| Stop::Failed(Error::ModuleNotLoaded(id)))
    }

    /// The instruction context values of `module` are evaluated at.
    fn address(&self, module: &RuntimeModule) -> Option<ImageAddress> {
        global_context_address(self.frame.resolved, module)
    }

    pub(super) fn context(&self, module: &RuntimeModule) -> crate::debug_info::VariableContext {
        variable_context(
            self.frame.stop_id,
            self.frame.pid,
            self.frame.resolved.id,
            module,
            self.address(module),
        )
    }

    /// A value as it is stored, without a view.
    pub(super) fn materialize(
        &mut self,
        module: &'a RuntimeModule,
        located: &Located,
    ) -> std::result::Result<InspectedValue, Stop> {
        let context = self.context(module);
        let mut runtime = self.frame.runtime(module);
        module
            .variables
            .materialize(located, context, &mut runtime, self.budget)
            .map_err(Stop::Failed)
    }

    fn accessed(
        accessed: crate::debug_info::Accessed,
        module: ModuleId,
    ) -> std::result::Result<StopPlace, Stop> {
        accessed
            .map(|located| StopPlace { module, located })
            .map_err(Stop::missing)
    }

    /// Any module's runtime, for memory the expression addresses directly.
    fn memory_module(&self) -> std::result::Result<&'a RuntimeModule, Stop> {
        self.frame.modules().next().ok_or_else(|| {
            Stop::Refused(Refusal::new(ErrorKind::Unsupported, "no module is loaded"))
        })
    }
}

impl<P: InspectionOps> TypeSource for StopMachine<'_, '_, P> {
    fn type_info(&self, ty: TypeReference) -> Option<TypeInfo> {
        self.frame.type_info(ty)
    }

    fn pointer_size(&self) -> u8 {
        self.frame.pointer_size()
    }

    fn byte_order(&self) -> ByteOrder {
        self.frame.byte_order()
    }
}

impl<P: InspectionOps> Machine for StopMachine<'_, '_, P> {
    type Object = StopObject;
    type Step = StopStep;
    type Place = StopPlace;

    fn charge(&mut self) -> std::result::Result<(), Stop> {
        self.budget
            .consume_expression_work(1)
            .map_err(|exhaustion| Stop::missing(VariableState::Unavailable(exhaustion.into())))?;
        let charges = self.frame.charges.get().wrapping_add(1);
        self.frame.charges.set(charges);
        if self.interruptible
            && charges.is_multiple_of(INTERRUPT_INTERVAL)
            && self.frame.controller.run_control_waiting()
        {
            return Err(Stop::Failed(Error::Interrupted));
        }
        Ok(())
    }

    fn locate(&mut self, object: &StopObject) -> std::result::Result<StopPlace, Stop> {
        let module = self.module(object.module)?;
        let address = self.address(module);
        let mut runtime = self.frame.runtime(module);
        let accessed = module
            .variables
            .locate(object.key, address, &mut runtime, self.budget)
            .map_err(Stop::Failed)?;
        Self::accessed(accessed, object.module)
    }

    fn check_indices(&self, step: &StopStep, indices: &[i128]) -> std::result::Result<(), Stop> {
        match step {
            StopStep::Provider { step, .. } => step
                .check_indices(indices)
                .map_err(|error| Stop::Refused(refusal(&error))),
            StopStep::Element(_) | StopStep::Global(_) => Ok(()),
        }
    }

    fn step(
        &mut self,
        from: &StopPlace,
        step: &StopStep,
        indices: &[i128],
    ) -> std::result::Result<StopPlace, Stop> {
        let (module_id, step) = match step {
            StopStep::Provider { module, step } => (*module, step),
            StopStep::Element(bound) => {
                let [index] = indices else {
                    return Err(Stop::Refused(Refusal::new(
                        ErrorKind::Type,
                        "a view's elements take one index",
                    )));
                };
                return self.view_element(bound, from, *index);
            }
            StopStep::Global(object) => return self.locate(object),
        };
        let module = self.module(module_id)?;
        let address = self.address(module);
        let mut runtime = self.frame.runtime(module);
        let accessed = module
            .variables
            .apply(
                &from.located,
                step,
                indices,
                address,
                &mut runtime,
                self.budget,
            )
            .map_err(|error| match error {
                Error::ValueIndexOutOfBounds { .. } => Stop::Refused(refusal(&error)),
                error => Stop::Failed(error),
            })?;
        Self::accessed(accessed, module_id)
    }

    fn place_at(
        &mut self,
        address: u64,
        ty: TypeReference,
    ) -> std::result::Result<StopPlace, Stop> {
        let module = type_module(self.frame.controller, ty).map_err(Stop::Refused)?;
        Ok(StopPlace {
            module: module.loaded.id,
            located: Located {
                ty: ty.id,
                storage: ValueStorage::Memory(VirtualAddress::new(address)),
            },
        })
    }

    fn address(&self, at: &StopPlace) -> std::result::Result<u64, Stop> {
        let not_in_memory = |where_: &str| {
            Stop::Refused(Refusal::new(
                ErrorKind::NotAnLvalue,
                format!("the value is {where_}, which has no address"),
            ))
        };
        match &at.located.storage {
            ValueStorage::Memory(address)
            | ValueStorage::Bytes {
                address: Some(address),
                ..
            } => Ok(address.get()),
            ValueStorage::Bytes {
                source: VariableValueSource::Register(register),
                ..
            } => Err(not_in_memory(&format!("in register {}", register.name))),
            ValueStorage::Bytes {
                source: VariableValueSource::Constant,
                ..
            } => Err(not_in_memory("a constant")),
            ValueStorage::Bytes { .. } => Err(not_in_memory("computed")),
            ValueStorage::ImplicitPointer { .. } => {
                Err(not_in_memory("optimized into its referent"))
            }
        }
    }

    fn load(&mut self, at: &StopPlace) -> std::result::Result<VariableValue, Stop> {
        let module = self.module(at.module)?;
        let context = self.context(module);
        let mut runtime = self.frame.runtime(module);
        module
            .variables
            .load(&at.located, context, &mut runtime, self.budget)
            .map_err(Stop::Failed)?
            .map_err(Stop::missing)
    }

    fn read(&mut self, address: u64, size: usize) -> std::result::Result<Vec<u8>, Stop> {
        use crate::debug_info::VariableRuntime as _;
        self.budget
            .consume_memory(size)
            .map_err(|exhaustion| Stop::missing(VariableState::Unavailable(exhaustion.into())))?;
        let module = self.memory_module()?;
        let mut runtime = self.frame.runtime(module);
        runtime
            .read_memory(VirtualAddress::new(address), size)
            .map(|bytes| bytes.to_vec())
            .map_err(|error| match error {
                crate::debug_info::VariableRuntimeError::Unavailable(reason) => {
                    Stop::missing(VariableState::Unavailable(reason))
                }
                crate::debug_info::VariableRuntimeError::Malformed(description)
                | crate::debug_info::VariableRuntimeError::Fatal(description) => {
                    Stop::Failed(Error::VariableRuntime(description))
                }
            })
    }

    fn text(&mut self, at: &StopPlace) -> std::result::Result<Option<TextSummary>, Stop> {
        let value = self.present(at)?;
        match value.state {
            VariableState::Available { text, .. } => Ok(text.map(|text| (*text).clone())),
            state => Err(Stop::missing(state)),
        }
    }

    fn length(&mut self, at: &StopPlace) -> std::result::Result<u64, Stop> {
        match self.load(at)? {
            VariableValue::Slice { length, .. } => Ok(length),
            _ => Err(Stop::Refused(Refusal::new(
                ErrorKind::Type,
                "the value is not a slice",
            ))),
        }
    }

    fn presented_length(&mut self, at: &StopPlace) -> std::result::Result<Option<u64>, Stop> {
        self.view_length(at)
    }

    fn register(&mut self, register: &Register) -> std::result::Result<u128, Stop> {
        let snapshot = self.frame.registers().ok_or_else(|| {
            Stop::missing(VariableState::Unavailable(
                VariableUnavailableReason::RegisterUnavailable("registers".into()),
            ))
        })?;
        let value = &snapshot.registers[usize::from(register.number)];
        let Some(bytes) = &value.bytes else {
            return Err(Stop::missing(VariableState::Unavailable(
                VariableUnavailableReason::RegisterNotSaved(Arc::clone(&value.register.name)),
            )));
        };
        let mut wide = [0_u8; 16];
        let length = bytes.len().min(16);
        wide[..length].copy_from_slice(&bytes[..length]);
        Ok(match self.frame.byte_order() {
            ByteOrder::Little => u128::from_le_bytes(wide),
            ByteOrder::Big => u128::from_be_bytes(wide) >> (8 * (16 - length)),
        })
    }

    fn present(&mut self, at: &StopPlace) -> std::result::Result<InspectedValue, Stop> {
        let module = self.module(at.module)?;
        let value = self.materialize(module, &at.located)?;
        self.presented(value)
    }

    fn present_bytes(
        &mut self,
        ty: TypeReference,
        bytes: &[u8],
    ) -> std::result::Result<InspectedValue, Stop> {
        let module = type_module(self.frame.controller, ty).map_err(Stop::Refused)?;
        let located = Located {
            ty: ty.id,
            storage: ValueStorage::Bytes {
                source: VariableValueSource::Computed,
                raw: Arc::from(bytes),
                start: 0,
                end: bytes.len(),
                address: None,
            },
        };
        let value = self.materialize(module, &located)?;
        self.presented(value)
    }

    fn present_pointer(
        &mut self,
        address: u64,
        pointee: Option<TypeReference>,
        type_info: TypeInfo,
    ) -> std::result::Result<InspectedValue, Stop> {
        let size = usize::from(self.pointer_size());
        let mut raw = address.to_le_bytes()[..size].to_vec();
        if self.byte_order() == ByteOrder::Big {
            raw.reverse();
        }
        let dereference = match (pointee, address) {
            (None, _) => DereferenceState::Unavailable {
                pointee: None,
                reason: DereferenceUnavailableReason::UnspecifiedPointee,
            },
            (Some(pointee), 0) => DereferenceState::Unavailable {
                pointee: self.type_info(pointee).map(Box::new),
                reason: DereferenceUnavailableReason::Null,
            },
            (Some(pointee), address) => match self.frame.controller.module_of(pointee) {
                Some(module) => DereferenceState::Available(DereferenceReference {
                    stop_id: self.frame.stop_id,
                    thread: super::debug_thread_id(self.frame.pid),
                    frame: self.frame.resolved.id,
                    module: module.loaded.id,
                    image: module.loaded.image,
                    context_address: self.address(module),
                    target_type: pointee.id,
                    target: DereferenceTarget::Address(VirtualAddress::new(address)),
                }),
                None => DereferenceState::Unavailable {
                    pointee: None,
                    reason: DereferenceUnavailableReason::UnspecifiedPointee,
                },
            },
        };
        Ok(self.finish(
            Some(type_info),
            VariableState::Available {
                source: VariableValueSource::Computed,
                raw: Some(raw.into()),
                value: VariableValue::Address(AddressValue {
                    address: VirtualAddress::new(address),
                }),
                dereference,
                children: ValueChildren::NotApplicable,
                text: None,
                presentation: None,
            },
        ))
    }

    fn finish(&self, type_info: Option<TypeInfo>, state: VariableState) -> InspectedValue {
        InspectedValue {
            type_info,
            state,
            completion: self.budget.completion(),
            usage: self.budget.usage(),
        }
    }
}

/// What evaluating an expression asks of the controller.
enum Evaluated {
    Done(Box<Evaluation>),
    /// Store `bytes` in `target`, then read it again.
    Write {
        target: StopPlace,
        bytes: Vec<u8>,
        whole: bool,
        span: Span,
    },
}

/// An interpreter failure as the debugger's error.
fn failure(failure: Failure) -> Error {
    match failure {
        Failure::Expression(error) => Error::Expression(error),
        Failure::Debugger(error) => error,
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Evaluates an expression that may assign, in one frame of a validated
    /// stop, and makes its assignment.
    pub(super) fn evaluate_assigning(
        &mut self,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
        expression: &Expression,
        limits: crate::InspectionLimits,
    ) -> Result<Evaluation> {
        let (target, bytes, whole, span) =
            match self.run_expression(stop_id, pid, frame, expression, Mode::Assign, limits)? {
                Evaluated::Done(evaluation) => return Ok(*evaluation),
                Evaluated::Write {
                    target,
                    bytes,
                    whole,
                    span,
                } => (target, bytes, whole, span),
            };
        let refused = |reason: String| {
            Error::Expression(crate::ExpressionError::new(
                ErrorKind::Assignment,
                span,
                reason,
            ))
        };
        match &target.located.storage {
            ValueStorage::Memory(address)
            | ValueStorage::Bytes {
                address: Some(address),
                ..
            } => {
                let written = self.write_memory_as(pid, *address, &bytes)?;
                if written != bytes.len() as u64 {
                    return Err(Error::MemoryNotWritable(*address));
                }
            }
            ValueStorage::Bytes {
                source: VariableValueSource::Register(register),
                ..
            } => {
                // A register belongs to the innermost frame; a caller's copy
                // lives in memory its callees saved, and part of a register
                // cannot be told from the whole.
                if frame != StackFrameId::INNERMOST {
                    return Err(refused(
                        "it is held in a register of a caller's frame".into(),
                    ));
                }
                if !whole {
                    return Err(refused("it is part of a value held in a register".into()));
                }
                self.write_register(pid, register.id, &bytes).map_err(|_| {
                    refused(format!("register {} cannot be changed", register.name))
                })?;
            }
            _ => {
                return Err(refused(
                    "the debug information computes it; it has no storage".into(),
                ));
            }
        }
        // The value is the target read again, so what the target's own type
        // makes of the stored bytes shows. It is read in the assignment's own
        // mode, which run control waiting cannot interrupt: the write is made,
        // and serving the request again would make it twice.
        let target = Expression::parse(
            expression
                .assignment_target()
                .unwrap_or_else(|| expression.text()),
        )
        .map_err(Error::Expression)?;
        self.evaluate(stop_id, pid, frame, &target, Mode::Assign, limits)
    }
}

impl<P: InspectionOps> Controller<P> {
    /// Evaluates an expression in one frame of a validated stop.
    pub(super) fn evaluate(
        &self,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
        expression: &Expression,
        mode: Mode,
        limits: crate::InspectionLimits,
    ) -> Result<Evaluation> {
        match self.run_expression(stop_id, pid, frame, expression, mode, limits)? {
            Evaluated::Done(evaluation) => Ok(*evaluation),
            Evaluated::Write { span, .. } => Err(Error::Expression(crate::ExpressionError::new(
                ErrorKind::Mode,
                span,
                "this process's state cannot be changed",
            ))),
        }
    }

    fn run_expression(
        &self,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
        expression: &Expression,
        mode: Mode,
        limits: crate::InspectionLimits,
    ) -> Result<Evaluated> {
        validate_inspection_limits(limits)?;
        let inferior = self.stopped_inferior(stop_id, pid)?;
        let resolved = self.resolve_frame(inferior, pid, frame)?;
        let scope = self.frame_for(inferior, stop_id, pid, &resolved);
        let program = bind(expression, &scope, mode).map_err(Error::Expression)?;
        let mut budget = InspectionBudget::new(limits);
        // Reading may wait for run control; an assignment, which changes
        // the stop, may not.
        let mut machine = StopMachine::new(&scope, &mut budget, mode == Mode::Read);
        let outcome = run(&program, &mut machine);
        record!("evaluate `{}`: {outcome:?}", expression.text());
        Ok(match outcome.map_err(failure)? {
            Outcome::Value { value, cause } => {
                Evaluated::Done(Box::new(Evaluation::Value { value, cause }))
            }
            Outcome::Range { base, start, end } => Evaluated::Done(Box::new(Evaluation::Range(
                self.range_page(stop_id, &base, start, end, &mut budget)?,
            ))),
            Outcome::Assign {
                target,
                bytes,
                whole,
                span,
            } => Evaluated::Write {
                target,
                bytes,
                whole,
                span,
            },
        })
    }

    /// Evaluates an expression where a breakpoint or watchpoint hit stopped
    /// one thread while others may run, as a condition or log message does:
    /// in the innermost frame the hit's stop `reason` presents, with
    /// capabilities that belong to no stop. A condition's value is its
    /// truth.
    pub(super) fn evaluate_at_hit(
        &self,
        pid: Pid,
        expression: &Expression,
        condition: bool,
        reason: &crate::StopReason,
    ) -> Result<Evaluation> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        // The thread is in the ptrace-stop that reported the hit, which its
        // recorded state does not reflect until the hit is resolved.
        validate_image_current(inferior)?;
        let mut budget = InspectionBudget::new(crate::InspectionLimits::default());
        let presentation = self.presentation_for_thread(pid, Some(reason))?;
        let resolved =
            self.resolve_presented_frame(inferior, pid, StackFrameId::INNERMOST, presentation)?;
        let scope = self.frame_for(inferior, StopId::new(0), pid, &resolved);
        let program = if condition {
            crate::eval::bind::bind_condition(expression, &scope)
        } else {
            bind(expression, &scope, Mode::Read)
        }
        .map_err(Error::Expression)?;
        // Run control is evaluating, so nothing interrupts it.
        let mut machine = StopMachine::new(&scope, &mut budget, false);
        let outcome = run(&program, &mut machine);
        record!("evaluate `{}` at a hit: {outcome:?}", expression.text());
        match outcome.map_err(failure)? {
            Outcome::Value { value, cause } => Ok(Evaluation::Value { value, cause }),
            _ => Err(Error::Expression(crate::ExpressionError::new(
                ErrorKind::Type,
                Span::new(0, expression.text().len()),
                "a breakpoint's expression must have a value",
            ))),
        }
    }

    /// Resolves an expression at a stop to the memory it occupies and the
    /// lifetime of that storage: the lifetime of the object it is part of,
    /// or none when it was reached through a pointer.
    pub(super) fn resolve_watch_target(
        &self,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
        expression: &Expression,
    ) -> Result<crate::WatchTarget> {
        let inferior = self.stopped_inferior(stop_id, pid)?;
        let resolved = self.resolve_frame(inferior, pid, frame)?;
        let scope = self.frame_for(inferior, stop_id, pid, &resolved);
        let program = bind(expression, &scope, Mode::Read).map_err(Error::Expression)?;
        let mut budget = InspectionBudget::new(crate::InspectionLimits::default());
        let mut machine = StopMachine::new(&scope, &mut budget, false);
        let Outcome::Value { value, .. } = run(&program, &mut machine).map_err(failure)? else {
            return Err(Error::WatchTargetUnsupported(
                "a range cannot be watched; watch one element".into(),
            ));
        };
        let (address, byte_size) = super::watchpoints::watchable_storage(&value)?;
        let (watch_scope, evidence) = match program.root_object() {
            Some(object) => {
                let module = self
                    .modules
                    .get(&object.module)
                    .ok_or(Error::ModuleNotLoaded(object.module))?;
                let storage = module.variables.object_storage(object.key);
                let local = object
                    .local
                    .then(|| scope.code.map(|(_, address, _)| address))
                    .flatten();
                self.root_watch_scope(inferior, pid, frame, object.module, storage, local)?
            }
            None => (crate::WatchScope::Location, None),
        };
        Ok(crate::WatchTarget {
            stop_id,
            expression: expression.clone(),
            address,
            byte_size,
            type_info: value.type_info,
            scope: watch_scope,
            frame: evidence,
        })
    }

    /// The type an expression has in one frame, reading no memory.
    pub(super) fn expression_type(
        &self,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
        expression: &Expression,
    ) -> Result<TypeInfo> {
        let inferior = self.stopped_inferior(stop_id, pid)?;
        let resolved = self.resolve_frame(inferior, pid, frame)?;
        let scope = self.frame_for(inferior, stop_id, pid, &resolved);
        let program = bind(expression, &scope, Mode::Read).map_err(Error::Expression)?;
        Ok(type_info(&scope, program.result()))
    }

    pub(super) fn frame_for<'a>(
        &'a self,
        inferior: &'a Inferior,
        stop_id: StopId,
        pid: Pid,
        resolved: &'a ResolvedFrame,
    ) -> Frame<'a, P> {
        Frame {
            controller: self,
            inferior,
            pid,
            stop_id,
            resolved,
            code: self.frame_scope(resolved),
            registers: OnceCell::new(),
            charges: Cell::new(0),
        }
    }
}
