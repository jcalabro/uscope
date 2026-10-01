//! Evaluating DWARF location expressions against a stopped thread.

use std::collections::HashMap;
use std::sync::Arc;

use gimli::{EvaluationResult, Location, Reader as _, RunTimeEndian, Value};

use crate::debug_info::dwarf::Reader;
use crate::debug_info::{VariableRuntime, VariableRuntimeError};
use crate::{
    BaseType, BaseTypeEncoding, ImageAddress, TargetDescription, VariableUnavailableReason,
    VariableValueSource, VirtualAddress,
};

use super::codec::{
    bytes_to_u64, dwarf_address_bytes, integer_bytes, register_u64, signed_integer_bytes,
    wrapping_integer_bytes,
};
use super::location::{EvaluationUnit, Expression, LocationDescription, LocationSelectionError};
use super::shape::ValueShapeError;
use super::types::DynamicAggregateLayoutKey;
use super::{
    ConstantValue, InspectionBudget, MAX_EVALUATION_ITERATIONS, MAX_LOCATION_PIECES, Metadata,
    MetadataAbsence,
};

pub(super) enum FrameBaseCache {
    Empty,
    Available(VirtualAddress),
    Unavailable(VariableUnavailableReason),
    Malformed(Arc<str>),
}

/// How an evaluation may obtain the frame base if the executed expression
/// path actually requires one.
pub(super) enum FrameBase<'a> {
    Unsupported,
    Lazy(FrameBaseContext<'a>),
}

pub(super) struct FrameBaseContext<'a> {
    pub(super) location: &'a Metadata<LocationDescription>,
    pub(super) address: Option<ImageAddress>,
    pub(super) cache: &'a mut FrameBaseCache,
}

/// An evaluation failure that preserves the unavailable-versus-malformed
/// distinction of the frame-base metadata it may consult.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum EvaluateError {
    Unavailable(VariableUnavailableReason),
    Malformed(Arc<str>),
    Fatal(Arc<str>),
}

impl From<VariableUnavailableReason> for EvaluateError {
    fn from(reason: VariableUnavailableReason) -> Self {
        Self::Unavailable(reason)
    }
}

impl From<VariableRuntimeError> for EvaluateError {
    fn from(error: VariableRuntimeError) -> Self {
        match error {
            VariableRuntimeError::Unavailable(reason) => Self::Unavailable(reason),
            VariableRuntimeError::Malformed(description) => Self::Malformed(description),
            VariableRuntimeError::Fatal(error) => Self::Fatal(error),
        }
    }
}

impl From<crate::InspectionExhaustion> for EvaluateError {
    fn from(exhaustion: crate::InspectionExhaustion) -> Self {
        Self::Unavailable(exhaustion.into())
    }
}

impl From<Arc<str>> for EvaluateError {
    fn from(description: Arc<str>) -> Self {
        Self::Malformed(description)
    }
}

impl From<&str> for EvaluateError {
    fn from(description: &str) -> Self {
        Self::Malformed(description.into())
    }
}

impl From<ValueShapeError> for EvaluateError {
    fn from(error: ValueShapeError) -> Self {
        match error {
            ValueShapeError::Malformed(description) => Self::Malformed(description),
            ValueShapeError::Unsupported(_) => {
                Self::Unavailable(crate::UnsupportedVariableFeature::TypeRepresentation.into())
            }
        }
    }
}

impl From<crate::UnsupportedVariableFeature> for EvaluateError {
    fn from(feature: crate::UnsupportedVariableFeature) -> Self {
        Self::Unavailable(feature.into())
    }
}

pub(super) fn resolve_frame_base(
    context: &mut FrameBaseContext<'_>,
    endian: RunTimeEndian,
    units: &[EvaluationUnit],
    runtime: &mut dyn VariableRuntime,
    budget: &mut InspectionBudget,
) -> std::result::Result<VirtualAddress, EvaluateError> {
    if matches!(context.cache, FrameBaseCache::Empty) {
        *context.cache = match context.location {
            Metadata::Value(frame_base) => match frame_base.expression(context.address) {
                Ok(Some(expression)) => {
                    match evaluate_frame_base(expression, endian, units, runtime, budget) {
                        Ok(value) => FrameBaseCache::Available(value),
                        // Request-specific exhaustion must not poison the
                        // frame-base cache shared by later inspections.
                        Err(EvaluateError::Unavailable(
                            reason @ (VariableUnavailableReason::EvaluationLimit
                            | VariableUnavailableReason::InspectionLimit(_)),
                        )) => return Err(reason.into()),
                        Err(EvaluateError::Unavailable(reason)) => {
                            FrameBaseCache::Unavailable(reason)
                        }
                        Err(EvaluateError::Malformed(description)) => {
                            FrameBaseCache::Malformed(description)
                        }
                        Err(error @ EvaluateError::Fatal(_)) => return Err(error),
                    }
                }
                Err(LocationSelectionError::Unavailable(reason)) => {
                    FrameBaseCache::Unavailable(reason)
                }
                Err(LocationSelectionError::Malformed(description)) => {
                    FrameBaseCache::Malformed(description)
                }
                Ok(None) => {
                    FrameBaseCache::Unavailable(VariableUnavailableReason::UnavailableAtInstruction)
                }
            },
            Metadata::Absent(
                MetadataAbsence::NoFrameBase
                | MetadataAbsence::NotApplicable
                | MetadataAbsence::NoLocation,
            ) => FrameBaseCache::Unavailable(VariableUnavailableReason::CallFrameUnavailable(
                crate::CallFrameUnavailableReason::NoInstructionContext,
            )),
            Metadata::Malformed(description) => FrameBaseCache::Malformed(Arc::clone(description)),
        };
    }
    match context.cache {
        FrameBaseCache::Available(value) => Ok(*value),
        FrameBaseCache::Unavailable(reason) => Err(EvaluateError::Unavailable(reason.clone())),
        FrameBaseCache::Malformed(description) => {
            Err(EvaluateError::Malformed(Arc::clone(description)))
        }
        FrameBaseCache::Empty => unreachable!("frame base cache was populated"),
    }
}

pub(super) fn evaluate_frame_base(
    expression: &Expression,
    endian: RunTimeEndian,
    units: &[EvaluationUnit],
    runtime: &mut dyn VariableRuntime,
    budget: &mut InspectionBudget,
) -> std::result::Result<VirtualAddress, EvaluateError> {
    // A frame-base expression may not itself require a frame base.
    let pieces = evaluate(
        expression,
        endian,
        &mut FrameBase::Unsupported,
        units,
        runtime,
        budget,
    )?;
    let [piece] = pieces.as_slice() else {
        return Err(EvaluateError::Malformed(
            "frame base is not one complete piece".into(),
        ));
    };
    if piece.size_in_bits.is_some() || piece.bit_offset.is_some() {
        return Err(EvaluateError::Malformed(
            "frame base is a partial piece".into(),
        ));
    }
    match piece.location {
        Location::Address { address } => Ok(VirtualAddress::new(address)),
        Location::Register { register } => {
            register_u64(runtime, register.0, endian).map(VirtualAddress::new)
        }
        _ => Err(EvaluateError::Malformed(
            "frame base did not evaluate to an address or register".into(),
        )),
    }
}

pub(super) fn evaluate<'expression>(
    expression: &'expression Expression,
    endian: RunTimeEndian,
    frame_base: &mut FrameBase<'_>,
    units: &[EvaluationUnit],
    runtime: &mut dyn VariableRuntime,
    budget: &mut InspectionBudget,
) -> std::result::Result<Vec<gimli::Piece<Reader<'expression>>>, EvaluateError> {
    evaluate_with_object(expression, endian, frame_base, units, runtime, budget, None)
}

pub(super) fn evaluate_dynamic_aggregate_address(
    layouts: &HashMap<DynamicAggregateLayoutKey, Expression>,
    key: DynamicAggregateLayoutKey,
    endian: RunTimeEndian,
    units: &[EvaluationUnit],
    runtime: &mut dyn VariableRuntime,
    budget: &mut InspectionBudget,
    object_address: VirtualAddress,
) -> std::result::Result<VirtualAddress, EvaluateError> {
    let expression = layouts.get(&key).ok_or_else(|| {
        EvaluateError::Unavailable(
            crate::UnsupportedVariableFeature::RuntimeAggregateLocation.into(),
        )
    })?;
    let pieces = evaluate_with_object(
        expression,
        endian,
        &mut FrameBase::Unsupported,
        units,
        runtime,
        budget,
        Some(object_address),
    )?;
    let [piece] = pieces.as_slice() else {
        return Err(EvaluateError::Malformed(
            "runtime aggregate child location produced multiple pieces".into(),
        ));
    };
    if piece.size_in_bits.is_some() || piece.bit_offset.is_some() {
        return Err(crate::UnsupportedVariableFeature::CompositeLocation.into());
    }
    let Location::Address { address } = piece.location else {
        return Err(EvaluateError::Malformed(
            "runtime aggregate child location did not produce an address".into(),
        ));
    };
    Ok(VirtualAddress::new(address))
}

/// The location an expression with no operations describes: an object the
/// compiler did not keep (DWARF 5 section 2.6.1.1.1).
fn empty_location<'expression>() -> Vec<gimli::Piece<Reader<'expression>>> {
    vec![gimli::Piece {
        size_in_bits: None,
        bit_offset: None,
        location: Location::Empty,
    }]
}

pub(super) fn evaluate_with_object<'expression>(
    expression: &'expression Expression,
    endian: RunTimeEndian,
    frame_base: &mut FrameBase<'_>,
    units: &[EvaluationUnit],
    runtime: &mut dyn VariableRuntime,
    budget: &mut InspectionBudget,
    object_address: Option<VirtualAddress>,
) -> std::result::Result<Vec<gimli::Piece<Reader<'expression>>>, EvaluateError> {
    budget.consume_expression_work(u64::from(MAX_EVALUATION_ITERATIONS))?;
    // gimli rejects an expression with no operations as a stack underflow.
    if expression.bytes.is_empty() {
        return Ok(empty_location());
    }
    let reader = gimli::EndianSlice::new(&expression.bytes, endian);
    let mut evaluation = gimli::Expression(reader).evaluation(expression.encoding);
    if let Some(address) = object_address {
        evaluation.set_initial_value(address.get());
        evaluation.set_object_address(address.get());
    }
    // Bound evaluation so a malformed expression with a backward branch cannot
    // hang the controller thread.
    evaluation.set_max_iterations(MAX_EVALUATION_ITERATIONS);
    let mut result = evaluation_step(evaluation.evaluate())?;
    loop {
        result = match result {
            EvaluationResult::Complete => return Ok(evaluation.result()),
            EvaluationResult::RequiresRegister {
                register,
                base_type,
            } => {
                let register = runtime.register(register.0)?;
                let value = evaluation_value(
                    &register.bytes,
                    evaluation_value_type(expression, units, base_type.0)?,
                    endian,
                )?;
                evaluation_step(evaluation.resume_with_register(value))?
            }
            EvaluationResult::RequiresFrameBase => {
                let value = match frame_base {
                    FrameBase::Unsupported => {
                        return Err("frame base is unavailable".into());
                    }
                    FrameBase::Lazy(context) => {
                        resolve_frame_base(context, endian, units, runtime, budget)?
                    }
                };
                evaluation_step(evaluation.resume_with_frame_base(value.get()))?
            }
            EvaluationResult::RequiresCallFrameCfa => evaluation_step(
                evaluation.resume_with_call_frame_cfa(runtime.call_frame_cfa()?.get()),
            )?,
            EvaluationResult::RequiresRelocatedAddress(address) => {
                evaluation_step(evaluation.resume_with_relocated_address(
                    runtime.relocate(ImageAddress::new(address))?.get(),
                ))?
            }
            EvaluationResult::RequiresIndexedAddress { index, relocate } => {
                let address = expression
                    .indexed_addresses
                    .get(&index.0)
                    .copied()
                    .ok_or_else(|| Arc::<str>::from("DWARF address index is unavailable"))?;
                let address = if relocate {
                    runtime.relocate(ImageAddress::new(address))?.get()
                } else {
                    address
                };
                evaluation_step(evaluation.resume_with_indexed_address(address))?
            }
            EvaluationResult::RequiresBaseType(offset) => evaluation_step(
                evaluation
                    .resume_with_base_type(evaluation_value_type(expression, units, offset.0)?),
            )?,
            EvaluationResult::RequiresMemory {
                address,
                size,
                space: None,
                base_type,
            } => {
                budget.consume_memory(usize::from(size))?;
                let bytes = runtime.read_memory(VirtualAddress::new(address), usize::from(size))?;
                let value = evaluation_value(
                    &bytes,
                    evaluation_value_type(expression, units, base_type.0)?,
                    endian,
                )?;
                evaluation_step(evaluation.resume_with_memory(value))?
            }
            EvaluationResult::RequiresMemory { space: Some(_), .. } => {
                return Err(crate::UnsupportedVariableFeature::AddressSpace.into());
            }
            EvaluationResult::RequiresEntryValue(_) => {
                return Err(crate::UnsupportedVariableFeature::EntryValue.into());
            }
            EvaluationResult::RequiresParameterRef(_) => {
                return Err(crate::UnsupportedVariableFeature::ParameterReference.into());
            }
            EvaluationResult::RequiresAtLocation(_) => {
                return Err(crate::UnsupportedVariableFeature::CrossDieEvaluation.into());
            }
            EvaluationResult::RequiresTls(offset) => {
                evaluation_step(evaluation.resume_with_tls(runtime.tls_address(offset)?.get()))?
            }
            EvaluationResult::RequiresWasmLocal { .. }
            | EvaluationResult::RequiresWasmGlobal { .. }
            | EvaluationResult::RequiresWasmStack { .. } => {
                return Err(crate::UnsupportedVariableFeature::WasmLocation.into());
            }
        };
    }
}

pub(super) fn evaluation_step<T>(
    result: std::result::Result<T, gimli::Error>,
) -> std::result::Result<T, EvaluateError> {
    result.map_err(|error| {
        if error == gimli::Error::TooManyIterations {
            return VariableUnavailableReason::EvaluationLimit.into();
        }
        evaluation_error(error).into()
    })
}

pub(super) fn evaluation_value_type(
    expression: &Expression,
    units: &[EvaluationUnit],
    offset: usize,
) -> std::result::Result<gimli::ValueType, VariableUnavailableReason> {
    if offset == 0 {
        return Ok(gimli::ValueType::Generic);
    }
    units
        .get(expression.unit)
        .and_then(|unit| unit.base_types.get(&offset))
        .copied()
        .ok_or_else(|| crate::UnsupportedVariableFeature::TypedValue.into())
}

pub(super) fn evaluation_value(
    bytes: &[u8],
    value_type: gimli::ValueType,
    endian: RunTimeEndian,
) -> std::result::Result<Value, EvaluateError> {
    if value_type == gimli::ValueType::Generic {
        return bytes_to_u64(bytes, endian).map(Value::Generic);
    }
    let size = usize::try_from(value_type.bit_size(u64::MAX) / 8).expect("value size fits usize");
    if bytes.len() < size {
        return Err(EvaluateError::Malformed(
            "register or memory value is shorter than its DWARF type".into(),
        ));
    }
    let bytes = match endian {
        RunTimeEndian::Little => &bytes[..size],
        RunTimeEndian::Big => &bytes[bytes.len() - size..],
    };
    Value::parse(value_type, gimli::EndianSlice::new(bytes, endian))
        .map_err(|error| EvaluateError::Malformed(evaluation_error(error)))
}

pub(super) fn materialize_pieces(
    pieces: &[gimli::Piece<Reader<'_>>],
    byte_size: u64,
    scalar_type: Option<&BaseType>,
    endian: RunTimeEndian,
    target: TargetDescription,
    runtime: &mut dyn VariableRuntime,
    budget: &mut InspectionBudget,
) -> std::result::Result<(VariableValueSource, Arc<[u8]>), EvaluateError> {
    if pieces.len() > MAX_LOCATION_PIECES {
        return Err(VariableUnavailableReason::EvaluationLimit.into());
    }
    let expected_bits = byte_size
        .checked_mul(8)
        .ok_or_else(|| EvaluateError::Malformed("scalar bit size overflow".into()))?;
    if let Some(reason) = incomplete_piece_reason(pieces, expected_bits)? {
        return Err(reason.into());
    }
    let [piece] = pieces else {
        return Err(crate::UnsupportedVariableFeature::CompositeLocation.into());
    };
    if piece.size_in_bits.is_some_and(|size| size != expected_bits) || piece.bit_offset.is_some() {
        return Err(crate::UnsupportedVariableFeature::CompositeLocation.into());
    }
    let size = usize::try_from(byte_size).expect("supported value size fits usize");
    match piece.location {
        Location::Empty => Err(VariableUnavailableReason::OptimizedOut(
            crate::OptimizedOutReason::EmptyLocation,
        )
        .into()),
        Location::Address { address } => {
            budget.consume_memory(size)?;
            runtime
                .read_memory(VirtualAddress::new(address), size)
                .map(|raw| {
                    (
                        VariableValueSource::Memory(VirtualAddress::new(address)),
                        raw,
                    )
                })
                .map_err(EvaluateError::from)
        }
        Location::Register { register } => {
            let register = runtime.register(register.0)?;
            let raw = object_bytes(&register.bytes, size, endian)?;
            Ok((VariableValueSource::Register(register.descriptor), raw))
        }
        Location::Value { value } => Ok((
            VariableValueSource::Computed,
            if let Some(type_info) = scalar_type {
                dwarf_value_bytes(value, type_info, target)?
            } else {
                dwarf_address_bytes(value, size, target)?
            },
        )),
        Location::Bytes { ref value } => {
            let bytes = value
                .to_slice()
                .map_err(|error| EvaluateError::Malformed(evaluation_error(error)))?
                .into_owned();
            if bytes.len() != size {
                return Err(EvaluateError::Malformed(
                    "implicit value size does not match its scalar type".into(),
                ));
            }
            Ok((VariableValueSource::Constant, bytes.into()))
        }
        Location::ImplicitPointer { .. } => {
            Err(crate::UnsupportedVariableFeature::ImplicitPointer.into())
        }
    }
}

pub(super) fn incomplete_piece_reason(
    pieces: &[gimli::Piece<Reader<'_>>],
    expected_bits: u64,
) -> std::result::Result<Option<VariableUnavailableReason>, EvaluateError> {
    if pieces.is_empty() {
        return Err(EvaluateError::Malformed(
            "DWARF location expression produced no pieces".into(),
        ));
    }
    let mut offset = 0_u64;
    let mut undefined = Vec::new();
    for piece in pieces {
        let size = match piece.size_in_bits {
            Some(size) => size,
            None if pieces.len() == 1 => expected_bits,
            None => {
                return Err(EvaluateError::Malformed(
                    "one of multiple DWARF location pieces has no size".into(),
                ));
            }
        };
        let end = offset.checked_add(size).ok_or_else(|| {
            EvaluateError::Malformed("DWARF location piece range overflows".into())
        })?;
        if end > expected_bits {
            return Err(EvaluateError::Malformed(
                "DWARF location pieces exceed the declared value size".into(),
            ));
        }
        if matches!(piece.location, Location::Empty) {
            undefined.push(crate::ValueBitRange { offset, size });
        }
        offset = end;
    }
    if undefined.is_empty() {
        return Ok(None);
    }
    if undefined.len() == 1 && undefined[0].offset == 0 && undefined[0].size == expected_bits {
        return Ok(Some(VariableUnavailableReason::OptimizedOut(
            crate::OptimizedOutReason::EmptyLocation,
        )));
    }
    Ok(Some(VariableUnavailableReason::OptimizedOut(
        crate::OptimizedOutReason::UndefinedPieces {
            ranges: undefined.into(),
        },
    )))
}

pub(super) fn object_bytes(
    bytes: &[u8],
    size: usize,
    endian: RunTimeEndian,
) -> std::result::Result<Arc<[u8]>, EvaluateError> {
    if bytes.len() < size {
        return Err(EvaluateError::Malformed(
            "register value is shorter than the scalar type".into(),
        ));
    }
    Ok(match endian {
        RunTimeEndian::Little => Arc::from(&bytes[..size]),
        RunTimeEndian::Big => Arc::from(&bytes[bytes.len() - size..]),
    })
}

pub(super) fn dwarf_value_bytes(
    value: Value,
    type_info: &BaseType,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, EvaluateError> {
    let size = usize::try_from(type_info.byte_size).expect("scalar size fits usize");
    let integer = match value {
        Value::Generic(value) | Value::U64(value) => Some(u128::from(value)),
        Value::U8(value) => Some(u128::from(value)),
        Value::U16(value) => Some(u128::from(value)),
        Value::U32(value) => Some(u128::from(value)),
        Value::I8(value) => Some(i128::from(value).cast_unsigned()),
        Value::I16(value) => Some(i128::from(value).cast_unsigned()),
        Value::I32(value) => Some(i128::from(value).cast_unsigned()),
        Value::I64(value) => Some(i128::from(value).cast_unsigned()),
        Value::F32(value) if size == 4 => {
            return integer_bytes(u128::from(value.to_bits()), size, target);
        }
        Value::F64(value) if size == 8 => {
            return integer_bytes(u128::from(value.to_bits()), size, target);
        }
        Value::F32(_) | Value::F64(_) => {
            return Err(EvaluateError::Malformed(
                "computed floating-point size mismatch".into(),
            ));
        }
    };
    let mut integer = integer.expect("integer DWARF values were classified above");
    // GCC and Clang represent optimized source booleans with word-sized
    // bitwise expressions (notably DW_OP_not). The source truth value is the
    // low bit after conversion to the declared one-byte boolean type.
    if type_info.encoding == BaseTypeEncoding::Boolean {
        integer &= 1;
    }
    wrapping_integer_bytes(integer, size, target)
}

pub(super) fn materialize_constant(
    value: &ConstantValue,
    size: usize,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, EvaluateError> {
    match value {
        // Producers use DW_FORM_sdata when the implicit high bits are signed.
        // A fixed data form supplies zero high bits; the target type then
        // interprets the materialized byte pattern.
        ConstantValue::Unsigned(value) | ConstantValue::Fixed(value) => {
            integer_bytes(*value, size, target)
        }
        ConstantValue::Signed(value) => signed_integer_bytes(*value, size, target),
        ConstantValue::Bytes(bytes) if bytes.len() == size => Ok(Arc::clone(bytes)),
        ConstantValue::Bytes(_) => Err(EvaluateError::Malformed(
            "constant value size does not match its scalar type".into(),
        )),
    }
}

pub(super) fn evaluation_error(error: gimli::Error) -> Arc<str> {
    format!("DWARF expression evaluation failed: {error}").into()
}
