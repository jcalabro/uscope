//! Decoding and encoding target integers, floats, addresses, and bit-fields.

use std::sync::Arc;

use gimli::{Reader as _, RunTimeEndian, Value};

use crate::debug_info::VariableRuntime;
use crate::debug_info::dwarf::Reader;
use crate::{
    Architecture, BaseType, BaseTypeEncoding, ByteOrder, FloatValue, IntegerValue, ScalarValue,
    TargetDescription, VariableInvalidReason, VariableUnavailableReason, VirtualAddress,
};

use super::evaluate::EvaluateError;
use super::inspect::ScalarDecodeError;

pub(super) fn integer_bit_width(base: &BaseType) -> std::result::Result<u32, Arc<str>> {
    let storage_bits = base
        .byte_size
        .checked_mul(8)
        .ok_or_else(|| Arc::from("integer storage bit width overflows"))?;
    let bits = base.bit_size.unwrap_or(storage_bits);
    if bits == 0 || bits > storage_bits || bits > 128 {
        return Err("integer bit width is outside its storage representation".into());
    }
    u32::try_from(bits).map_err(|_| Arc::from("integer bit width exceeds u32"))
}

pub(super) fn checked_integer_value(
    value: IntegerValue,
    base: &BaseType,
) -> std::result::Result<IntegerValue, Arc<str>> {
    let bits = integer_bit_width(base)?;
    match (base.encoding, value) {
        (
            BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter,
            IntegerValue::Signed(value),
        ) => {
            if bits < 128 {
                let minimum = -(1_i128 << (bits - 1));
                let maximum = (1_i128 << (bits - 1)) - 1;
                if !(minimum..=maximum).contains(&value) {
                    return Err("signed enumerator does not fit its representation".into());
                }
            }
            Ok(IntegerValue::Signed(value))
        }
        (
            BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter,
            IntegerValue::Unsigned(value),
        ) => {
            let value = i128::try_from(value)
                .map_err(|_| Arc::from("enumerator does not fit its signed representation"))?;
            checked_integer_value(IntegerValue::Signed(value), base)
        }
        (
            BaseTypeEncoding::Boolean
            | BaseTypeEncoding::Unsigned
            | BaseTypeEncoding::UnsignedCharacter,
            IntegerValue::Unsigned(value),
        ) => {
            if bits < 128 && value >= 1_u128 << bits {
                return Err("unsigned enumerator does not fit its representation".into());
            }
            if matches!(base.encoding, BaseTypeEncoding::Boolean) && value > 1 {
                return Err("boolean enumerator is neither zero nor one".into());
            }
            Ok(IntegerValue::Unsigned(value))
        }
        (
            BaseTypeEncoding::Boolean
            | BaseTypeEncoding::Unsigned
            | BaseTypeEncoding::UnsignedCharacter,
            IntegerValue::Signed(value),
        ) => {
            let value = u128::try_from(value)
                .map_err(|_| Arc::from("negative enumerator has an unsigned representation"))?;
            checked_integer_value(IntegerValue::Unsigned(value), base)
        }
        (BaseTypeEncoding::Floating, _) => {
            Err("enumerator representation is floating-point".into())
        }
    }
}

pub(super) fn decode_integer_value(
    base: &BaseType,
    bytes: &[u8],
    byte_order: ByteOrder,
) -> std::result::Result<IntegerValue, Arc<str>> {
    let expected = usize::try_from(base.byte_size)
        .map_err(|_| Arc::from("integer byte size does not fit host usize"))?;
    if bytes.len() != expected {
        return Err("integer storage size mismatch".into());
    }
    let bits = integer_bit_width(base)?;
    let raw = unsigned_value(bytes, byte_order).map_err(|reason| Arc::from(reason.to_string()))?
        & low_bits_mask(usize::try_from(bits).expect("bit width fits usize"));
    let value = match base.encoding {
        BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter => {
            let signed = if bits == 128 {
                raw.cast_signed()
            } else {
                let shift = 128 - bits;
                (raw << shift).cast_signed() >> shift
            };
            IntegerValue::Signed(signed)
        }
        BaseTypeEncoding::Boolean
        | BaseTypeEncoding::Unsigned
        | BaseTypeEncoding::UnsignedCharacter => IntegerValue::Unsigned(raw),
        BaseTypeEncoding::Floating => {
            return Err("integer representation is floating-point".into());
        }
    };
    checked_integer_value(value, base)
}

pub(super) fn enumeration_constant(
    value: gimli::AttributeValue<Reader<'_>>,
    base: &BaseType,
    byte_order: ByteOrder,
) -> std::result::Result<IntegerValue, Arc<str>> {
    let signed = matches!(
        base.encoding,
        BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter
    );
    let value = match value {
        gimli::AttributeValue::Data1(value) if signed => {
            IntegerValue::Signed(i128::from(value.cast_signed()))
        }
        gimli::AttributeValue::Data2(value) if signed => {
            IntegerValue::Signed(i128::from(value.cast_signed()))
        }
        gimli::AttributeValue::Data4(value) if signed => {
            IntegerValue::Signed(i128::from(value.cast_signed()))
        }
        gimli::AttributeValue::Data8(value) if signed => {
            IntegerValue::Signed(i128::from(value.cast_signed()))
        }
        gimli::AttributeValue::Data16(value) if signed => IntegerValue::Signed(value.cast_signed()),
        gimli::AttributeValue::Data1(value) => IntegerValue::Unsigned(u128::from(value)),
        gimli::AttributeValue::Data2(value) => IntegerValue::Unsigned(u128::from(value)),
        gimli::AttributeValue::Data4(value) => IntegerValue::Unsigned(u128::from(value)),
        gimli::AttributeValue::Data8(value) | gimli::AttributeValue::Udata(value) => {
            IntegerValue::Unsigned(u128::from(value))
        }
        gimli::AttributeValue::Data16(value) => IntegerValue::Unsigned(value),
        gimli::AttributeValue::Sdata(value) => IntegerValue::Signed(i128::from(value)),
        gimli::AttributeValue::Block(value) => {
            let bytes = value
                .to_slice()
                .map_err(|error| Arc::from(error.to_string()))?;
            return decode_integer_value(base, bytes.as_ref(), byte_order);
        }
        _ => return Err("enumerator constant has an unsupported form".into()),
    };
    checked_integer_value(value, base)
}

pub(super) fn read_uleb128_u128(
    bytes: &[u8],
    cursor: &mut usize,
) -> std::result::Result<u128, Arc<str>> {
    let mut value = 0_u128;
    let mut shift = 0_u32;
    loop {
        let byte = *bytes
            .get(*cursor)
            .ok_or_else(|| Arc::from("truncated unsigned discriminant LEB128"))?;
        *cursor = cursor
            .checked_add(1)
            .ok_or_else(|| Arc::from("discriminant-list cursor overflows"))?;
        let payload = u128::from(byte & 0x7f);
        if shift >= 128 {
            if payload != 0 {
                return Err("unsigned discriminant LEB128 exceeds 128 bits".into());
            }
        } else {
            let available = 128 - shift;
            if available < 7 && payload >= 1_u128 << available {
                return Err("unsigned discriminant LEB128 exceeds 128 bits".into());
            }
            value |= payload << shift;
        }
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift = shift
            .checked_add(7)
            .ok_or_else(|| Arc::from("unsigned discriminant LEB128 shift overflows"))?;
        if shift > 133 {
            return Err("unsigned discriminant LEB128 is overlong".into());
        }
    }
}

pub(super) fn read_sleb128_i128(
    bytes: &[u8],
    cursor: &mut usize,
) -> std::result::Result<i128, Arc<str>> {
    let mut value = 0_u128;
    let mut shift = 0_u32;
    loop {
        let byte = *bytes
            .get(*cursor)
            .ok_or_else(|| Arc::from("truncated signed discriminant LEB128"))?;
        *cursor = cursor
            .checked_add(1)
            .ok_or_else(|| Arc::from("discriminant-list cursor overflows"))?;
        let payload = u128::from(byte & 0x7f);
        if shift < 128 {
            let available = 128 - shift;
            value |= (payload & low_bits_mask(usize::try_from(available.min(7)).unwrap())) << shift;
        }
        if byte & 0x80 == 0 {
            let negative = byte & 0x40 != 0;
            if shift >= 128 {
                let canonical = if negative {
                    payload == 0x7f
                } else {
                    payload == 0
                };
                if !canonical {
                    return Err("signed discriminant LEB128 exceeds 128 bits".into());
                }
            } else {
                let consumed = shift + 7;
                if negative && consumed < 128 {
                    value |= u128::MAX << consumed;
                } else if consumed > 128 {
                    let used = 128 - shift;
                    let high = payload >> used;
                    let expected = if negative {
                        low_bits_mask(usize::try_from(7 - used).unwrap())
                    } else {
                        0
                    };
                    if high != expected {
                        return Err("signed discriminant LEB128 exceeds 128 bits".into());
                    }
                }
            }
            return Ok(value.cast_signed());
        }
        shift = shift
            .checked_add(7)
            .ok_or_else(|| Arc::from("signed discriminant LEB128 shift overflows"))?;
        if shift > 133 {
            return Err("signed discriminant LEB128 is overlong".into());
        }
    }
}

pub(super) fn compare_integer_values(
    left: IntegerValue,
    right: IntegerValue,
) -> std::result::Result<std::cmp::Ordering, Arc<str>> {
    match (left, right) {
        (IntegerValue::Signed(left), IntegerValue::Signed(right)) => Ok(left.cmp(&right)),
        (IntegerValue::Unsigned(left), IntegerValue::Unsigned(right)) => Ok(left.cmp(&right)),
        _ => Err("variant selectors mix signed and unsigned values".into()),
    }
}

pub(super) fn extract_bit_field(
    bytes: &[u8],
    bit_offset: u64,
    bit_size: u64,
    byte_order: ByteOrder,
) -> std::result::Result<u128, Arc<str>> {
    let end = bit_offset
        .checked_add(bit_size)
        .ok_or_else(|| Arc::from("bit-field range overflows"))?;
    let available = u64::try_from(bytes.len())
        .ok()
        .and_then(|length| length.checked_mul(8))
        .ok_or_else(|| Arc::from("record storage size overflows"))?;
    if bit_size == 0 || bit_size > 128 || end > available {
        return Err("bit-field range is outside its containing object".into());
    }
    let mut value = 0_u128;
    for field_bit in 0..bit_size {
        let source = bit_offset + field_bit;
        let byte = bytes[usize::try_from(source / 8).expect("validated bit index fits usize")];
        let within = u32::try_from(source % 8).expect("bit index is below eight");
        let bit = match byte_order {
            ByteOrder::Little => (byte >> within) & 1,
            ByteOrder::Big => (byte >> (7 - within)) & 1,
        };
        match byte_order {
            ByteOrder::Little => value |= u128::from(bit) << field_bit,
            ByteOrder::Big => value = (value << 1) | u128::from(bit),
        }
    }
    Ok(value)
}

pub(super) fn wrapping_integer_bytes(
    value: u128,
    size: usize,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, EvaluateError> {
    if size == 0 || size > 16 {
        return Err(crate::UnsupportedVariableFeature::ScalarRepresentation.into());
    }
    let value = value & low_bits_mask(size * 8);
    let bytes = match target.byte_order {
        ByteOrder::Little => value.to_le_bytes()[..size].to_vec(),
        ByteOrder::Big => value.to_be_bytes()[16 - size..].to_vec(),
    };
    Ok(bytes.into())
}

pub(super) fn dwarf_address_bytes(
    value: Value,
    size: usize,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, EvaluateError> {
    let address = match value {
        Value::Generic(value) | Value::U64(value) => value,
        Value::U8(value) => u64::from(value),
        Value::U16(value) => u64::from(value),
        Value::U32(value) => u64::from(value),
        Value::I8(value) => i64::from(value).cast_unsigned(),
        Value::I16(value) => i64::from(value).cast_unsigned(),
        Value::I32(value) => i64::from(value).cast_unsigned(),
        Value::I64(value) => value.cast_unsigned(),
        Value::F32(_) | Value::F64(_) => {
            return Err(EvaluateError::Malformed(
                "floating-point value cannot represent an address".into(),
            ));
        }
    };
    wrapping_integer_bytes(u128::from(address), size, target)
}

pub(super) fn decode_address(
    raw: &[u8],
    byte_size: u64,
    target: TargetDescription,
) -> std::result::Result<VirtualAddress, EvaluateError> {
    let size =
        usize::try_from(byte_size).map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
    if size == 0 || size > 8 {
        return Err(crate::UnsupportedVariableFeature::ScalarRepresentation.into());
    }
    if raw.len() != size {
        return Err(EvaluateError::Malformed(
            "pointer storage size does not match its declared type".into(),
        ));
    }
    let mut bytes = [0_u8; 8];
    match target.byte_order {
        ByteOrder::Little => bytes[..size].copy_from_slice(raw),
        ByteOrder::Big => bytes[8 - size..].copy_from_slice(raw),
    }
    let value = match target.byte_order {
        ByteOrder::Little => u64::from_le_bytes(bytes),
        ByteOrder::Big => u64::from_be_bytes(bytes),
    };
    Ok(VirtualAddress::new(value))
}

pub(super) fn integer_bytes(
    value: u128,
    size: usize,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, EvaluateError> {
    if size == 0 || size > 16 || (size < 16 && value >= (1_u128 << (size * 8))) {
        return Err(EvaluateError::Malformed(
            "constant value does not fit its scalar type".into(),
        ));
    }
    let bytes = match target.byte_order {
        ByteOrder::Little => value.to_le_bytes()[..size].to_vec(),
        ByteOrder::Big => value.to_be_bytes()[16 - size..].to_vec(),
    };
    Ok(bytes.into())
}

pub(super) fn signed_integer_bytes(
    value: i128,
    size: usize,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, EvaluateError> {
    if size == 0 || size > 16 {
        return Err(crate::UnsupportedVariableFeature::ScalarRepresentation.into());
    }
    let bits = size * 8;
    if bits < 128 {
        let minimum = -(1_i128 << (bits - 1));
        let maximum = (1_i128 << (bits - 1)) - 1;
        if !(minimum..=maximum).contains(&value) {
            return Err(EvaluateError::Malformed(
                "signed constant value does not fit its scalar type".into(),
            ));
        }
    }
    integer_bytes(value.cast_unsigned() & low_bits_mask(bits), size, target)
}

pub(super) const fn low_bits_mask(bits: usize) -> u128 {
    if bits == 128 {
        u128::MAX
    } else {
        (1_u128 << bits) - 1
    }
}

pub(super) fn register_u64(
    runtime: &mut dyn VariableRuntime,
    register: u16,
    endian: RunTimeEndian,
) -> std::result::Result<u64, EvaluateError> {
    let value = runtime.register(register)?;
    bytes_to_u64(&value.bytes, endian)
}

pub(super) fn bytes_to_u64(
    bytes: &[u8],
    endian: RunTimeEndian,
) -> std::result::Result<u64, EvaluateError> {
    if bytes.len() > 8 {
        return Err(EvaluateError::Malformed(
            "DWARF expression requested more than one word".into(),
        ));
    }
    let mut word = [0_u8; 8];
    match endian {
        RunTimeEndian::Little => word[..bytes.len()].copy_from_slice(bytes),
        RunTimeEndian::Big => word[8 - bytes.len()..].copy_from_slice(bytes),
    }
    Ok(match endian {
        RunTimeEndian::Little => u64::from_le_bytes(word),
        RunTimeEndian::Big => u64::from_be_bytes(word),
    })
}

pub(super) fn decode_scalar(
    type_info: &BaseType,
    bytes: &[u8],
    target: TargetDescription,
) -> std::result::Result<ScalarValue, ScalarDecodeError> {
    let expected = usize::try_from(type_info.byte_size).expect("scalar byte size fits usize");
    if bytes.len() != expected {
        return Err(ScalarDecodeError::Malformed(
            "scalar storage size mismatch".into(),
        ));
    }
    match type_info.encoding {
        BaseTypeEncoding::Boolean => {
            let bits = integer_bit_width(type_info).map_err(ScalarDecodeError::Malformed)?;
            let value = unsigned_value(bytes, target.byte_order)
                .map_err(ScalarDecodeError::Malformed)?
                & low_bits_mask(usize::try_from(bits).expect("bit width fits usize"));
            match value {
                0 => Ok(ScalarValue::Boolean(false)),
                1 => Ok(ScalarValue::Boolean(true)),
                value => Err(ScalarDecodeError::Invalid(
                    VariableInvalidReason::BooleanRepresentation(value),
                )),
            }
        }
        BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter => {
            let IntegerValue::Signed(value) =
                decode_integer_value(type_info, bytes, target.byte_order)
                    .map_err(ScalarDecodeError::Malformed)?
            else {
                unreachable!("signed encoding returns a signed integer");
            };
            Ok(ScalarValue::Signed(value))
        }
        BaseTypeEncoding::Unsigned | BaseTypeEncoding::UnsignedCharacter => {
            let IntegerValue::Unsigned(value) =
                decode_integer_value(type_info, bytes, target.byte_order)
                    .map_err(ScalarDecodeError::Malformed)?
            else {
                unreachable!("unsigned encoding returns an unsigned integer");
            };
            Ok(ScalarValue::Unsigned(value))
        }
        BaseTypeEncoding::Floating => {
            decode_float(type_info, bytes, target).map(ScalarValue::Floating)
        }
    }
}

pub(super) fn unsigned_value(
    bytes: &[u8],
    byte_order: ByteOrder,
) -> std::result::Result<u128, Arc<str>> {
    if bytes.is_empty() || bytes.len() > 16 {
        return Err("unsupported integer storage size".into());
    }
    let mut value = [0_u8; 16];
    match byte_order {
        ByteOrder::Little => value[..bytes.len()].copy_from_slice(bytes),
        ByteOrder::Big => value[16 - bytes.len()..].copy_from_slice(bytes),
    }
    Ok(match byte_order {
        ByteOrder::Little => u128::from_le_bytes(value),
        ByteOrder::Big => u128::from_be_bytes(value),
    })
}

pub(super) fn decode_float(
    type_info: &BaseType,
    bytes: &[u8],
    target: TargetDescription,
) -> std::result::Result<FloatValue, ScalarDecodeError> {
    match bytes.len() {
        4 => Ok(FloatValue::Binary32(
            u32::try_from(
                unsigned_value(bytes, target.byte_order).map_err(ScalarDecodeError::Malformed)?,
            )
            .expect("four bytes fit u32"),
        )),
        8 => Ok(FloatValue::Binary64(
            u64::try_from(
                unsigned_value(bytes, target.byte_order).map_err(ScalarDecodeError::Malformed)?,
            )
            .expect("eight bytes fit u64"),
        )),
        // x87 extended precision is padded to 16 bytes, the same size as the
        // IEEE binary128 of `__float128` and `f128`; only the name tells them
        // apart.
        16 if target.architecture == Architecture::X86_64
            && target.byte_order == ByteOrder::Little
            && matches!(
                type_info.base_name.as_ref(),
                "long double" | "__float80" | "_Float64x" | "f80" | "c_longdouble"
            ) =>
        {
            Ok(FloatValue::X87Extended {
                significand: u64::from_le_bytes(bytes[..8].try_into().expect("eight-byte slice")),
                sign_exponent: u16::from_le_bytes(bytes[8..10].try_into().expect("two-byte slice")),
            })
        }
        _ => Err(ScalarDecodeError::Unavailable(
            crate::UnsupportedVariableFeature::ScalarRepresentation.into(),
        )),
    }
}
