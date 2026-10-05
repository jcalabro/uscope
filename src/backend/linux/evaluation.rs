//! Expressions at a stop: a frame's names for binding, and its state for
//! running, over the debug-info providers' structural primitives.

use std::cell::OnceCell;
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
use crate::eval::syntax::Expression;
use crate::eval::syntax::ast::Tag;
use crate::eval::target::{
    Lookup, Machine, Planned, Refusal, Register, Scope, StepKind, Stop, TypeLookup, TypeQuery,
};
use crate::eval::types::{TypeSource, c_type_key_of_name, type_info};
use crate::inspection::InspectionBudget;
use crate::model::{DereferenceTarget, ValueStorage};
use crate::protocol::StopId;
use crate::{
    AddressValue, ByteOrder, CodeInstanceId, DereferenceReference, DereferenceState,
    DereferenceUnavailableReason, Error, ImageAddress, InspectedValue, IntegerValue, ModuleId,
    PointerWidth, RecordKind, RegisterSnapshot, Result, StackFrameId, TextSummary, TypeInfo,
    TypeKind, TypeNode, TypeReference, ValueChildren, VariableState, VariableUnavailableReason,
    VariableValue, VariableValueSource, VirtualAddress,
};

use super::frames::{FrameRegisters, ResolvedFrame};
use super::inspection::{global_context_address, validate_inspection_limits, variable_context};
use super::native::InspectionOps;
use super::registers::x86_64_register_snapshot;
use super::{
    Controller, Inferior, RuntimeModule, validate_image_current, validate_public_stop,
    validate_stopped_thread,
};

/// A data object a frame names.
#[derive(Debug, Clone, Copy)]
pub(super) struct StopObject {
    module: ModuleId,
    key: ObjectKey,
}

/// A structural step planned in one module's image.
#[derive(Clone)]
pub(super) struct StopStep {
    module: ModuleId,
    step: PlannedStep,
}

impl fmt::Debug for StopStep {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "StopStep({:?})", self.module)
    }
}

/// Where a value of one module's types is.
#[derive(Debug, Clone)]
pub(super) struct StopPlace {
    module: ModuleId,
    located: Located,
}

/// One frame of a stop, as expressions see it.
struct Frame<'a, P: InspectionOps> {
    controller: &'a Controller<P>,
    inferior: &'a Inferior,
    pid: Pid,
    stop_id: StopId,
    resolved: &'a ResolvedFrame,
    /// The frame's module, address, and inline instance, when it has debug
    /// information.
    code: Option<(&'a RuntimeModule, ImageAddress, Option<CodeInstanceId>)>,
    registers: OnceCell<Option<RegisterSnapshot>>,
}

impl<'a, P: InspectionOps> Frame<'a, P> {
    fn module(&self, id: ModuleId) -> Option<&'a RuntimeModule> {
        self.controller.modules.get(&id)
    }

    fn module_of(&self, ty: TypeReference) -> Option<&'a RuntimeModule> {
        self.controller
            .modules
            .values()
            .find(|module| module.loaded.image == ty.image)
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

    const fn reference(module: &RuntimeModule, id: crate::TypeId) -> TypeReference {
        TypeReference {
            image: module.loaded.image,
            id,
        }
    }
}

impl<P: InspectionOps> TypeSource for Frame<'_, P> {
    fn type_info(&self, ty: TypeReference) -> Option<TypeInfo> {
        self.module_of(ty)?.image.type_info(ty).cloned()
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

fn integer(value: IntegerValue) -> Exact {
    match value {
        IntegerValue::Signed(value) => Exact::from(value),
        IntegerValue::Unsigned(value) => Exact::from(value),
    }
}

/// A provider's refusal of a step, as an expression error.
fn refusal(error: &Error) -> Refusal {
    let kind = match error {
        Error::MemberNotFound { .. }
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

impl<P: InspectionOps> Scope for Frame<'_, P> {
    type Object = StopObject;
    type Step = StopStep;

    #[expect(
        clippy::too_many_lines,
        reason = "locals, globals, and enumerators are looked up in one order"
    )]
    fn lookup(
        &self,
        name: &str,
        outermost: bool,
    ) -> std::result::Result<Lookup<StopObject>, Refusal> {
        if !outermost && let Some((module, address, selected)) = self.code {
            match module.variables.visible_object(address, selected, name) {
                Ok(key) => {
                    return Ok(Lookup::Object {
                        object: StopObject {
                            module: module.loaded.id,
                            key,
                        },
                        ty: module
                            .variables
                            .object_type(key)
                            .map(|id| Self::reference(module, id)),
                    });
                }
                Err(Error::VariableNotFound(_)) => {}
                Err(Error::AmbiguousVariable(_)) => {
                    return Ok(Lookup::Ambiguous(vec![name.to_owned()]));
                }
                Err(error) => return Err(refusal(&error)),
            }
        }

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
                return Ok(Lookup::Object {
                    object: StopObject {
                        module: module.loaded.id,
                        key,
                    },
                    ty: module
                        .variables
                        .object_type(key)
                        .map(|id| Self::reference(module, id)),
                });
            }
            ([], true) => {}
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
                return Ok(Lookup::Ambiguous(candidates.into_iter().collect()));
            }
        }

        // Enumerators, by their own name or qualified by their
        // enumeration's, in the frame's module before others.
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
                        found.push((qualified, integer(enumerator.value), info.reference));
                    }
                }
            }
            found.sort_by(|left, right| left.0.cmp(&right.0));
            found.dedup_by(|left, right| left.0 == right.0 && left.1 == right.1);
            match found.as_slice() {
                [] => {}
                [(_, value, ty)] => {
                    return Ok(Lookup::Enumerator {
                        value: *value,
                        ty: *ty,
                    });
                }
                _ => {
                    return Ok(Lookup::Ambiguous(
                        found.into_iter().map(|(qualified, ..)| qualified).collect(),
                    ));
                }
            }
        }
        Ok(Lookup::NotFound)
    }

    fn lookup_type(&self, query: &TypeQuery) -> TypeLookup {
        for module in self.modules() {
            let mut found: Vec<(String, TypeReference)> = Vec::new();
            for node in module.image.types() {
                let TypeNode::Resolved(info) = node else {
                    continue;
                };
                let named = info.name.as_ref() == query.name
                    || (matches!(info.kind, TypeKind::Base(_))
                        && c_type_key_of_name(&info.name).is_some_and(|key| key == query.name));
                if named && tag_matches(query.tag, &info.kind) {
                    found.push((definition(&module.image, info), info.reference));
                }
            }
            found.sort_by(|left, right| left.0.cmp(&right.0));
            found.dedup_by(|left, right| left.0 == right.0);
            match found.as_slice() {
                [] => {}
                [(_, ty)] => return TypeLookup::Found(*ty),
                _ => {
                    return TypeLookup::Ambiguous(
                        found
                            .into_iter()
                            .map(|(definition, _)| definition)
                            .collect(),
                    );
                }
            }
        }
        TypeLookup::NotFound
    }

    fn plan(
        &self,
        from: TypeReference,
        step: StepKind<'_>,
    ) -> std::result::Result<Planned<StopStep>, Refusal> {
        let module = self.module_of(from).ok_or_else(|| {
            Refusal::new(ErrorKind::Unsupported, "the type's module is not loaded")
        })?;
        let step = match step {
            StepKind::Deref => Step::Deref,
            StepKind::Member(name) => Step::Member(name),
            StepKind::Index { available } => Step::Index { available },
        };
        let planned = module
            .variables
            .plan_step(from.id, step)
            .map_err(|error| refusal(&error))?;
        Ok(Planned {
            result: planned.result().map(|id| Self::reference(module, id)),
            consumed: planned.consumed(),
            step: StopStep {
                module: module.loaded.id,
                step: planned,
            },
        })
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
struct StopMachine<'a, 'b, P: InspectionOps> {
    frame: &'b Frame<'a, P>,
    budget: &'b mut InspectionBudget,
}

const fn failed(error: Error) -> Stop {
    Stop::Failed(error)
}

impl<'a, P: InspectionOps> StopMachine<'a, '_, P> {
    fn module(&self, id: ModuleId) -> std::result::Result<&'a RuntimeModule, Stop> {
        self.frame
            .module(id)
            .ok_or_else(|| failed(Error::ModuleNotLoaded(id)))
    }

    /// The instruction context values of `module` are evaluated at.
    fn address(&self, module: &RuntimeModule) -> Option<ImageAddress> {
        global_context_address(self.frame.resolved, module)
    }

    fn context(&self, module: &RuntimeModule) -> crate::debug_info::VariableContext {
        variable_context(
            self.frame.stop_id,
            self.frame.pid,
            self.frame.resolved.id,
            module,
            self.address(module),
        )
    }

    fn materialize(
        &mut self,
        module: &RuntimeModule,
        located: &Located,
    ) -> std::result::Result<InspectedValue, Stop> {
        let context = self.context(module);
        let mut runtime = self.frame.controller.frame_runtime(
            self.frame.inferior,
            self.frame.pid,
            self.frame.resolved,
            module,
        );
        module
            .variables
            .materialize(located, context, &mut runtime, self.budget)
            .map_err(failed)
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
            .map_err(|exhaustion| Stop::missing(VariableState::Unavailable(exhaustion.into())))
    }

    fn locate(&mut self, object: &StopObject) -> std::result::Result<StopPlace, Stop> {
        let module = self.module(object.module)?;
        let address = self.address(module);
        let mut runtime = self.frame.controller.frame_runtime(
            self.frame.inferior,
            self.frame.pid,
            self.frame.resolved,
            module,
        );
        let accessed = module
            .variables
            .locate(object.key, address, &mut runtime, self.budget)
            .map_err(failed)?;
        Self::accessed(accessed, object.module)
    }

    fn check_indices(&self, step: &StopStep, indices: &[i128]) -> std::result::Result<(), Stop> {
        step.step
            .check_indices(indices)
            .map_err(|error| Stop::Refused(refusal(&error)))
    }

    fn step(
        &mut self,
        from: &StopPlace,
        step: &StopStep,
        indices: &[i128],
    ) -> std::result::Result<StopPlace, Stop> {
        let module = self.module(step.module)?;
        let address = self.address(module);
        let mut runtime = self.frame.controller.frame_runtime(
            self.frame.inferior,
            self.frame.pid,
            self.frame.resolved,
            module,
        );
        let accessed = module
            .variables
            .apply(
                &from.located,
                &step.step,
                indices,
                address,
                &mut runtime,
                self.budget,
            )
            .map_err(|error| match error {
                Error::ValueIndexOutOfBounds { .. } => Stop::Refused(refusal(&error)),
                error => failed(error),
            })?;
        Self::accessed(accessed, step.module)
    }

    fn place_at(
        &mut self,
        address: u64,
        ty: TypeReference,
    ) -> std::result::Result<StopPlace, Stop> {
        let module = self.frame.module_of(ty).ok_or_else(|| {
            Stop::Refused(Refusal::new(
                ErrorKind::Unsupported,
                "the type's module is not loaded",
            ))
        })?;
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
        let mut runtime = self.frame.controller.frame_runtime(
            self.frame.inferior,
            self.frame.pid,
            self.frame.resolved,
            module,
        );
        module
            .variables
            .load(&at.located, context, &mut runtime, self.budget)
            .map_err(failed)?
            .map_err(Stop::missing)
    }

    fn read(&mut self, address: u64, size: usize) -> std::result::Result<Vec<u8>, Stop> {
        use crate::debug_info::VariableRuntime as _;
        self.budget
            .consume_memory(size)
            .map_err(|exhaustion| Stop::missing(VariableState::Unavailable(exhaustion.into())))?;
        let module = self.memory_module()?;
        let mut runtime = self.frame.controller.frame_runtime(
            self.frame.inferior,
            self.frame.pid,
            self.frame.resolved,
            module,
        );
        runtime
            .read_memory(VirtualAddress::new(address), size)
            .map(|bytes| bytes.to_vec())
            .map_err(|error| match error {
                crate::debug_info::VariableRuntimeError::Unavailable(reason) => {
                    Stop::missing(VariableState::Unavailable(reason))
                }
                crate::debug_info::VariableRuntimeError::Malformed(description)
                | crate::debug_info::VariableRuntimeError::Fatal(description) => {
                    failed(Error::VariableRuntime(description))
                }
            })
    }

    fn text(&mut self, at: &StopPlace) -> std::result::Result<Option<TextSummary>, Stop> {
        let module = self.module(at.module)?;
        let value = self.materialize(module, &at.located)?;
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
        self.materialize(module, &at.located)
    }

    fn present_bytes(
        &mut self,
        ty: TypeReference,
        bytes: &[u8],
    ) -> std::result::Result<InspectedValue, Stop> {
        let module = self.frame.module_of(ty).ok_or_else(|| {
            Stop::Refused(Refusal::new(
                ErrorKind::Unsupported,
                "the type's module is not loaded",
            ))
        })?;
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
        self.materialize(module, &located)
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
            (Some(pointee), address) => match self.frame.module_of(pointee) {
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
        validate_inspection_limits(limits)?;
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_stopped_thread(inferior, pid)?;
        validate_image_current(inferior)?;
        let resolved = self.resolve_frame(inferior, pid, frame)?;
        let mut budget = InspectionBudget::new(limits);
        self.evaluate_in_frame(
            inferior,
            stop_id,
            pid,
            &resolved,
            expression,
            mode,
            &mut budget,
        )
    }

    /// The type an expression has in one frame, reading no memory.
    pub(super) fn expression_type(
        &self,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
        expression: &Expression,
    ) -> Result<TypeInfo> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_stopped_thread(inferior, pid)?;
        validate_image_current(inferior)?;
        let resolved = self.resolve_frame(inferior, pid, frame)?;
        let scope = self.frame_for(inferior, stop_id, pid, &resolved);
        let program = bind(expression, &scope, Mode::Read).map_err(Error::Expression)?;
        Ok(type_info(&scope, program.result()))
    }

    fn frame_for<'a>(
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
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "evaluation keeps the stop, frame, expression, mode, and budget explicit"
    )]
    pub(super) fn evaluate_in_frame(
        &self,
        inferior: &Inferior,
        stop_id: StopId,
        pid: Pid,
        resolved: &ResolvedFrame,
        expression: &Expression,
        mode: Mode,
        budget: &mut InspectionBudget,
    ) -> Result<Evaluation> {
        let scope = self.frame_for(inferior, stop_id, pid, resolved);
        let program = bind(expression, &scope, mode).map_err(Error::Expression)?;
        let mut machine = StopMachine {
            frame: &scope,
            budget,
        };
        let outcome = run(&program, &mut machine);
        record!("evaluate `{}`: {outcome:?}", expression.text());
        match outcome {
            Ok(Outcome::Value { value, cause }) => Ok(Evaluation::Value { value, cause }),
            Ok(Outcome::Range { base, start, end }) => Ok(Evaluation::Range { base, start, end }),
            Err(Failure::Expression(error)) => Err(Error::Expression(error)),
            Err(Failure::Debugger(error)) => Err(error),
        }
    }
}
