//! The values a function returned, read where its caller sees them the
//! instant the call returns. Go's register ABI (`abi-internal.md`) is the
//! one convention known: optimized code keeps no result in a place its
//! debug information describes by then, but the convention says where
//! each one is.
//!
//! Go assigns results to registers apart from the arguments, starting
//! again from the first: an integer, pointer, or boolean takes the next
//! integer register, a float the next floating-point one, a complex number
//! two, and a string, slice, interface, struct, or one-element array its
//! parts in order. A value with no register left for every part, or an
//! array of more elements, is on the stack instead, after the arguments
//! the stack holds, at the caller's stack pointer.

use std::sync::Arc;

use crate::debug_info::{Located, ReturnedValue, VariableRuntime};
use crate::inspection::InspectionBudget;
use crate::model::ValueStorage;
use crate::{
    BaseTypeEncoding, ImageAddress, RecordMemberLayout, Result, TypeId, TypeKind, TypeNode,
    VariableKind, VariableMalformedKind, VariableValueSource, VirtualAddress,
};

use super::evaluate::EvaluateError;
use super::generic::Generic;
use super::inspect::evaluate_error_state;
use super::types::TypeResolution;
use super::{CatalogDataObject, DwarfVariableInfo};

/// The DWARF numbers of x86-64's integer result registers, in order: rax,
/// rbx, rcx, rdi, rsi, and r8 through r11.
const INTEGER: [u16; 9] = [0, 3, 2, 5, 4, 8, 9, 10, 11];
/// The DWARF numbers of x86-64's floating-point result registers, xmm0
/// through xmm14.
const FLOATING: [u16; 15] = [17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31];
/// x86-64's stack pointer.
const STACK_POINTER: u16 = 7;
/// The size of a word, and the stack's alignment between arguments and
/// results.
const WORD: u64 = 8;

/// How a function returns its values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReturnConvention {
    /// Go's register ABI on x86-64, which the producer names `regabi`.
    GoRegisters,
}

/// Where one part of a value is.
#[derive(Debug, Clone, Copy)]
enum Source {
    /// The low bytes of a register.
    Register(u16),
    /// The stack, this far past the caller's stack pointer.
    Stack(u64),
}

/// One part of a value: where it is in the value, its size, and where it
/// was returned.
#[derive(Debug, Clone, Copy)]
struct Part {
    offset: u64,
    size: u64,
    source: Source,
}

/// Why a value has no registers.
enum Unassigned {
    /// It does not fit those left, so it is on the stack.
    Stack,
    /// Its type is one the convention does not describe.
    Unsupported,
    /// Its type's debug information is malformed.
    Malformed(Arc<str>),
}

/// One assignment of values to registers and the stack, in order.
#[derive(Default)]
struct Assignment {
    integers: usize,
    floats: usize,
    stack: u64,
}

impl DwarfVariableInfo {
    pub(super) fn returned_values(
        &self,
        function: ImageAddress,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Option<Vec<ReturnedValue>>> {
        let Some(catalog) = self.function_at(function) else {
            return Ok(None);
        };
        let Some(ReturnConvention::GoRegisters) = catalog.returns else {
            return Ok(None);
        };
        let own = |kind: VariableKind| {
            catalog
                .objects
                .iter()
                .map(|&index| &self.objects[index])
                .filter(move |object| {
                    object.kind == kind && object.instance.is_none() && object.lexical_depth == 0
                })
        };
        let results = own(VariableKind::Result).collect::<Vec<_>>();
        // The stack-assigned results follow the stack-assigned arguments,
        // so where they are depends on every argument.
        let mut arguments = Assignment::default();
        for parameter in own(VariableKind::Parameter) {
            let assigned = type_of(parameter)
                .ok_or_else(|| unknown_type("argument", parameter))
                .and_then(|ty| arguments.assign(self, ty, &mut Vec::new()));
            if let Err(error) = assigned {
                return Self::all_missing(&results, &error).map(Some);
            }
        }
        let mut assignment = Assignment {
            stack: arguments.stack.next_multiple_of(WORD),
            ..Assignment::default()
        };
        let mut returned = Vec::with_capacity(results.len());
        for (index, result) in results.iter().enumerate() {
            // So does where each result is on every one before it.
            let Some(ty) = type_of(result) else {
                let missing =
                    Self::all_missing(&results[index..], &unknown_type("result", result))?;
                returned.extend(missing);
                break;
            };
            let mut parts = Vec::new();
            let captured = assignment
                .assign(self, ty, &mut parts)
                .and_then(|()| self.capture(ty, &parts, runtime, budget));
            let value = match captured {
                Ok(located) => Ok(located),
                Err(error) => Err(evaluate_error_state(
                    error,
                    VariableMalformedKind::InvalidTypeGraph,
                )?),
            };
            // A generic result's shape is laid out as its type is, but its
            // type is in a dictionary the returned call took away.
            let unresolved_shape = match self.generic_type(ty, None, None, runtime, budget)? {
                Generic::Unresolved(_, reason) => Some(reason),
                Generic::Plain | Generic::Resolved(_) => None,
            };
            returned.push(ReturnedValue {
                name: Arc::clone(&result.name),
                ty: Some(ty),
                value,
                unresolved_shape,
            });
        }
        Ok(Some(returned))
    }

    /// Results none of whose values can be found, for one reason.
    fn all_missing(
        results: &[&CatalogDataObject],
        error: &EvaluateError,
    ) -> Result<Vec<ReturnedValue>> {
        let state = evaluate_error_state(error.clone(), VariableMalformedKind::InvalidTypeGraph)?;
        Ok(results
            .iter()
            .map(|result| ReturnedValue {
                name: Arc::clone(&result.name),
                ty: type_of(result),
                value: Err(state.clone()),
                unresolved_shape: None,
            })
            .collect())
    }

    /// The bytes of a value of type `ty` from its parts.
    fn capture(
        &self,
        ty: TypeId,
        parts: &[Part],
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<Located, EvaluateError> {
        let size = to_usize(self.size(ty).map_err(placed)?);
        let mut raw = vec![0; size];
        let mut stack_pointer = None;
        let mut on_stack = None;
        for part in parts {
            let length = to_usize(part.size);
            let bytes = match part.source {
                Source::Register(register) => runtime.register(register)?.bytes,
                Source::Stack(offset) => {
                    let base = match stack_pointer {
                        Some(base) => base,
                        None => {
                            *stack_pointer.insert(word(&runtime.register(STACK_POINTER)?.bytes))
                        }
                    };
                    let address = VirtualAddress::new(base.wrapping_add(offset));
                    on_stack = Some(address);
                    budget.consume_memory(length)?;
                    runtime.read_memory(address, length)?
                }
            };
            let start = to_usize(part.offset);
            let (Some(target), Some(source)) = (
                raw.get_mut(start..start.saturating_add(length)),
                bytes.get(..length),
            ) else {
                return Err("a returned value's part lies outside it".into());
            };
            target.copy_from_slice(source);
        }
        let source = match (parts, on_stack) {
            ([_], Some(address)) => VariableValueSource::Memory(address),
            _ => VariableValueSource::Pieces,
        };
        Ok(Located {
            ty,
            storage: ValueStorage::Bytes {
                source,
                raw: raw.into(),
                start: 0,
                end: size,
                address: None,
                unavailable: Arc::new([]),
            },
        })
    }

    /// What `ty` is, through names and modifiers.
    fn underlying(&self, ty: TypeId) -> std::result::Result<&crate::TypeInfo, Unassigned> {
        let mut id = ty;
        for _ in 0..64 {
            let Some(TypeNode::Resolved(info)) = self.types.get(id.index()) else {
                return Err(Unassigned::Malformed("a type is malformed".into()));
            };
            match &info.kind {
                TypeKind::Named {
                    target: Some(target),
                    ..
                }
                | TypeKind::Modified { target, .. } => id = target.id,
                _ => return Ok(info),
            }
        }
        Err(Unassigned::Malformed("a type names itself".into()))
    }

    fn size(&self, ty: TypeId) -> std::result::Result<u64, Unassigned> {
        self.underlying(ty)?
            .byte_size
            .ok_or_else(|| Unassigned::Malformed("a type has no size".into()))
    }

    /// The alignment a value of type `ty` takes on the stack.
    fn alignment(&self, ty: TypeId) -> std::result::Result<u64, Unassigned> {
        let info = self.underlying(ty)?;
        Ok(match &info.kind {
            TypeKind::Base(base) if base.encoding == BaseTypeEncoding::ComplexFloating => {
                base.byte_size / 2
            }
            TypeKind::Base(base) => base.byte_size,
            TypeKind::Enumeration { representation, .. } => representation.byte_size,
            TypeKind::Pointer { .. } | TypeKind::Function | TypeKind::Slice { .. } => WORD,
            TypeKind::Array { element, .. } => self.alignment(element.id)?,
            TypeKind::Record { members, .. } => {
                let mut alignment = 1;
                for member in members.iter() {
                    alignment = alignment.max(self.alignment(member.type_ref.id)?);
                }
                alignment
            }
            _ => return Err(Unassigned::Unsupported),
        }
        .max(1))
    }
}

impl Assignment {
    /// Assigns a value of type `ty` registers, or else the stack, and adds
    /// where its parts are to `parts`.
    fn assign(
        &mut self,
        info: &DwarfVariableInfo,
        ty: TypeId,
        parts: &mut Vec<Part>,
    ) -> std::result::Result<(), EvaluateError> {
        let (integers, floats, assigned) = (self.integers, self.floats, parts.len());
        match self.registers(info, ty, 0, parts) {
            Ok(()) => Ok(()),
            Err(Unassigned::Stack) => {
                (self.integers, self.floats) = (integers, floats);
                parts.truncate(assigned);
                let size = info.size(ty).map_err(placed)?;
                let offset = self
                    .stack
                    .next_multiple_of(info.alignment(ty).map_err(placed)?);
                parts.push(Part {
                    offset: 0,
                    size,
                    source: Source::Stack(offset),
                });
                self.stack = offset + size;
                Ok(())
            }
            Err(unassigned) => Err(placed(unassigned)),
        }
    }

    /// Assigns registers to a value of type `ty` at `offset` in the value
    /// being returned.
    fn registers(
        &mut self,
        info: &DwarfVariableInfo,
        ty: TypeId,
        offset: u64,
        parts: &mut Vec<Part>,
    ) -> std::result::Result<(), Unassigned> {
        let resolved = info.underlying(ty)?;
        match &resolved.kind {
            TypeKind::Base(base) => match base.encoding {
                BaseTypeEncoding::Floating => self.float(offset, base.byte_size, parts),
                BaseTypeEncoding::ComplexFloating => {
                    let half = base.byte_size / 2;
                    self.float(offset, half, parts)?;
                    self.float(offset + half, half, parts)
                }
                _ => self.integer(offset, base.byte_size, parts),
            },
            TypeKind::Enumeration { representation, .. } => {
                self.integer(offset, representation.byte_size, parts)
            }
            TypeKind::Pointer { .. } | TypeKind::Function => self.integer(offset, WORD, parts),
            TypeKind::Slice { has_capacity, .. } => {
                let words = if *has_capacity { 3 } else { 2 };
                (0..words).try_for_each(|word| self.integer(offset + word * WORD, WORD, parts))
            }
            TypeKind::Array {
                element,
                dimensions,
            } => match dimensions
                .iter()
                .try_fold(1_u64, |count, dimension| count.checked_mul(dimension.count))
            {
                Some(0) => Ok(()),
                Some(1) => self.registers(info, element.id, offset, parts),
                _ => Err(Unassigned::Stack),
            },
            TypeKind::Record {
                members,
                bases,
                incomplete: false,
                ..
            } if bases.is_empty() => members.iter().try_for_each(|member| {
                let RecordMemberLayout::ByteOffset(at) = member.layout else {
                    return Err(Unassigned::Malformed(
                        "a struct's field has no byte offset".into(),
                    ));
                };
                self.registers(info, member.type_ref.id, offset + at, parts)
            }),
            _ => Err(Unassigned::Unsupported),
        }
    }

    fn integer(
        &mut self,
        offset: u64,
        size: u64,
        parts: &mut Vec<Part>,
    ) -> std::result::Result<(), Unassigned> {
        if size > WORD {
            return Err(Unassigned::Unsupported);
        }
        let register = *INTEGER.get(self.integers).ok_or(Unassigned::Stack)?;
        self.integers += 1;
        parts.push(Part {
            offset,
            size,
            source: Source::Register(register),
        });
        Ok(())
    }

    fn float(
        &mut self,
        offset: u64,
        size: u64,
        parts: &mut Vec<Part>,
    ) -> std::result::Result<(), Unassigned> {
        if size > WORD {
            return Err(Unassigned::Unsupported);
        }
        let register = *FLOATING.get(self.floats).ok_or(Unassigned::Stack)?;
        self.floats += 1;
        parts.push(Part {
            offset,
            size,
            source: Source::Register(register),
        });
        Ok(())
    }
}

/// Why a value that cannot take registers has no place on the stack
/// either.
fn placed(unassigned: Unassigned) -> EvaluateError {
    match unassigned {
        Unassigned::Unsupported => crate::UnsupportedVariableFeature::TypeRepresentation.into(),
        Unassigned::Malformed(description) => EvaluateError::Malformed(description),
        Unassigned::Stack => "a type has no stack layout".into(),
    }
}

/// The type of a parameter or result, unless its debug information is
/// malformed.
const fn type_of(object: &CatalogDataObject) -> Option<TypeId> {
    match (&object.malformed, &object.type_info) {
        (None, TypeResolution::Resolved(id)) => Some(*id),
        _ => None,
    }
}

fn unknown_type(what: &str, object: &CatalogDataObject) -> EvaluateError {
    format!("the type of {what} {} is unknown", object.name)
        .as_str()
        .into()
}

fn word(bytes: &[u8]) -> u64 {
    let mut word = [0; 8];
    for (target, source) in word.iter_mut().zip(bytes) {
        *target = *source;
    }
    u64::from_le_bytes(word)
}

fn to_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}
