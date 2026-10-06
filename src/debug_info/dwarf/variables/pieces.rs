//! Values a location describes in pieces (`DW_OP_piece`, `DW_OP_bit_piece`):
//! a value assembled from registers, memory, and implicit values, with the
//! bits no piece holds kept unavailable rather than filled.

use std::sync::Arc;

use gimli::{Location, Reader as _, RunTimeEndian, Value};

use crate::debug_info::VariableRuntime;
use crate::debug_info::dwarf::Reader;
use crate::inspection::InspectionBudget;
use crate::model::{UnavailableBits, ValueStorage};
use crate::{
    OptimizedOutReason, ValueBitRange, VariableUnavailableReason, VariableValueSource,
    VirtualAddress,
};

use super::evaluate::EvaluateError;

/// Whether `pieces` describe a value of `expected_bits` whole: one piece,
/// the whole size, from its source's first bit.
pub(super) fn is_whole(pieces: &[gimli::Piece<Reader<'_>>], expected_bits: u64) -> bool {
    matches!(pieces, [piece] if piece.bit_offset.is_none()
        && piece.size_in_bits.is_none_or(|size| size == expected_bits))
}

/// Assembles a value of `byte_size` bytes from its pieces. Memory pieces
/// that lay the whole value out contiguously stay memory, which keeps the
/// value addressable; any other value is captured as bytes, with the bits
/// of missing pieces, and of pieces whose source cannot be read, marked
/// unavailable with their reasons.
pub(super) fn assemble(
    pieces: &[gimli::Piece<Reader<'_>>],
    byte_size: u64,
    endian: RunTimeEndian,
    runtime: &mut dyn VariableRuntime,
    budget: &mut InspectionBudget,
) -> std::result::Result<ValueStorage, EvaluateError> {
    // Pieces number bits from the least significant, which is the
    // least-addressed bit only in little-endian order.
    if endian != RunTimeEndian::Little {
        return Err(crate::UnsupportedVariableFeature::CompositeLocation.into());
    }
    let expected_bits = byte_size
        .checked_mul(8)
        .ok_or(VariableUnavailableReason::EvaluationLimit)?;
    let placed = placements(pieces, expected_bits)?;
    if let Some(address) = contiguous_memory(&placed, expected_bits) {
        return Ok(ValueStorage::Memory(address));
    }
    let size =
        usize::try_from(byte_size).map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
    let mut raw = vec![0_u8; size];
    let mut unavailable = Vec::new();
    let mut covered = 0;
    for (offset, piece) in placed {
        let size = piece_size(piece, expected_bits);
        let range = ValueBitRange { offset, size };
        covered = offset + size;
        match source_bits(piece, size, runtime, budget)? {
            Ok((bytes, first)) => copy_bits(&mut raw, offset, &bytes, first, size),
            Err(reason) => unavailable.push(UnavailableBits { range, reason }),
        }
    }
    // Bits past the last piece are not part of any piece: DWARF leaves them
    // undefined.
    if covered < expected_bits {
        unavailable.push(UnavailableBits {
            range: ValueBitRange {
                offset: covered,
                size: expected_bits - covered,
            },
            reason: VariableUnavailableReason::OptimizedOut(OptimizedOutReason::EmptyLocation),
        });
    }
    let missing: u64 = unavailable.iter().map(|bits| bits.range.size).sum();
    if missing == expected_bits {
        return Err(unavailable_within(&unavailable, 0, size)
            .expect("a value with no available bits has an unavailable read")
            .into());
    }
    Ok(ValueStorage::Bytes {
        source: VariableValueSource::Pieces,
        raw: raw.into(),
        start: 0,
        end: size,
        address: None,
        unavailable: unavailable.into(),
    })
}

/// Each piece with the bit offset in the value where it begins, checked to
/// fit the value.
fn placements<'p, 'r>(
    pieces: &'p [gimli::Piece<Reader<'r>>],
    expected_bits: u64,
) -> std::result::Result<Vec<(u64, &'p gimli::Piece<Reader<'r>>)>, EvaluateError> {
    if pieces.is_empty() {
        return Err(EvaluateError::Malformed(
            "DWARF location expression produced no pieces".into(),
        ));
    }
    if pieces.len() > 1 && pieces.iter().any(|piece| piece.size_in_bits.is_none()) {
        return Err(EvaluateError::Malformed(
            "one of multiple DWARF location pieces has no size".into(),
        ));
    }
    let mut offset = 0_u64;
    let mut placed = Vec::with_capacity(pieces.len());
    for piece in pieces {
        let end = offset
            .checked_add(piece_size(piece, expected_bits))
            .filter(|end| *end <= expected_bits)
            .ok_or_else(|| {
                EvaluateError::Malformed(
                    "DWARF location pieces exceed the declared value size".into(),
                )
            })?;
        placed.push((offset, piece));
        offset = end;
    }
    Ok(placed)
}

fn piece_size(piece: &gimli::Piece<Reader<'_>>, expected_bits: u64) -> u64 {
    piece.size_in_bits.unwrap_or(expected_bits)
}

/// The address of a value whose pieces are whole bytes of memory, laid out
/// as the value is.
fn contiguous_memory(
    placed: &[(u64, &gimli::Piece<Reader<'_>>)],
    expected_bits: u64,
) -> Option<VirtualAddress> {
    let mut first = None;
    let mut covered = 0;
    for (offset, piece) in placed {
        let Location::Address { address } = piece.location else {
            return None;
        };
        let size = piece_size(piece, expected_bits);
        if piece.bit_offset.is_some_and(|bits| bits != 0)
            || !offset.is_multiple_of(8)
            || !size.is_multiple_of(8)
        {
            return None;
        }
        let start = *first.get_or_insert(address);
        if start.checked_add(offset / 8) != Some(address) {
            return None;
        }
        covered = offset + size;
    }
    (covered == expected_bits).then(|| VirtualAddress::new(first.expect("pieces were placed")))
}

/// The bytes holding one piece's bits and the bit within them where the
/// piece begins, or why they cannot be read.
type SourceBits = std::result::Result<(Arc<[u8]>, u64), VariableUnavailableReason>;

/// Reads one piece's source.
fn source_bits(
    piece: &gimli::Piece<Reader<'_>>,
    size: u64,
    runtime: &mut dyn VariableRuntime,
    budget: &mut InspectionBudget,
) -> std::result::Result<SourceBits, EvaluateError> {
    let first = piece.bit_offset.unwrap_or(0);
    let end = first
        .checked_add(size)
        .ok_or(VariableUnavailableReason::EvaluationLimit)?;
    let bytes = match &piece.location {
        Location::Empty => {
            return Ok(Err(VariableUnavailableReason::OptimizedOut(
                OptimizedOutReason::EmptyLocation,
            )));
        }
        Location::Register { register } => match runtime.register(register.0) {
            Ok(register) => register.bytes,
            Err(error) => return unavailable(error.into()),
        },
        Location::Address { address } => {
            let length = usize::try_from(end.div_ceil(8))
                .map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
            budget.consume_memory(length)?;
            match runtime.read_memory(VirtualAddress::new(*address), length) {
                Ok(bytes) => bytes,
                Err(error) => return unavailable(error.into()),
            }
        }
        Location::Value { value } => value_bytes(*value).into(),
        Location::Bytes { value } => value
            .to_slice()
            .map_err(|error| EvaluateError::Malformed(error.to_string().into()))?
            .into_owned()
            .into(),
        Location::ImplicitPointer { .. } => {
            return Ok(Err(
                crate::UnsupportedVariableFeature::ImplicitPointer.into()
            ));
        }
    };
    if u64::try_from(bytes.len())
        .ok()
        .is_none_or(|length| length.saturating_mul(8) < end)
    {
        return Err(EvaluateError::Malformed(
            "a DWARF location piece is larger than its source".into(),
        ));
    }
    Ok(Ok((bytes, first)))
}

/// An unavailable source, or the malformed or fatal failure it was.
fn unavailable(error: EvaluateError) -> std::result::Result<SourceBits, EvaluateError> {
    match error {
        EvaluateError::Unavailable(reason) => Ok(Err(reason)),
        error => Err(error),
    }
}

/// A DWARF stack value's bytes, least significant first.
fn value_bytes(value: Value) -> Vec<u8> {
    match value {
        Value::Generic(value) | Value::U64(value) => value.to_le_bytes().to_vec(),
        Value::I8(value) => value.to_le_bytes().to_vec(),
        Value::U8(value) => value.to_le_bytes().to_vec(),
        Value::I16(value) => value.to_le_bytes().to_vec(),
        Value::U16(value) => value.to_le_bytes().to_vec(),
        Value::I32(value) => value.to_le_bytes().to_vec(),
        Value::U32(value) => value.to_le_bytes().to_vec(),
        Value::I64(value) => value.to_le_bytes().to_vec(),
        Value::F32(value) => value.to_le_bytes().to_vec(),
        Value::F64(value) => value.to_le_bytes().to_vec(),
    }
}

/// Copies `size` bits from `source`, beginning at bit `first`, into
/// `destination` at bit `offset`, numbering bits from the least
/// significant bit of the first byte.
fn copy_bits(destination: &mut [u8], offset: u64, source: &[u8], first: u64, size: u64) {
    for bit in 0..size {
        let from = first + bit;
        let to = offset + bit;
        let value =
            (source[usize::try_from(from / 8).expect("bit index fits usize")] >> (from % 8)) & 1;
        let byte = &mut destination[usize::try_from(to / 8).expect("bit index fits usize")];
        let mask = 1_u8 << (to % 8);
        *byte = (*byte & !mask) | (value << (to % 8));
    }
}

/// Why `size` bytes at byte `start` of captured bytes cannot be read, when
/// any of their bits is unavailable: the bits optimized out, relative to
/// `start`, when nothing else is wrong, or else the first other reason.
pub(super) fn unavailable_within(
    unavailable: &[UnavailableBits],
    start: usize,
    size: usize,
) -> Option<VariableUnavailableReason> {
    let first = u64::try_from(start).ok()?.checked_mul(8)?;
    let end = first.checked_add(u64::try_from(size).ok()?.checked_mul(8)?)?;
    let mut missing = Vec::new();
    for bits in unavailable {
        let from = bits.range.offset.max(first);
        let to = bits.range.offset.saturating_add(bits.range.size).min(end);
        if from >= to {
            continue;
        }
        if !matches!(bits.reason, VariableUnavailableReason::OptimizedOut(_)) {
            return Some(bits.reason.clone());
        }
        match missing.last_mut() {
            Some(ValueBitRange { offset, size }) if *offset + *size == from - first => {
                *size += to - from;
            }
            _ => missing.push(ValueBitRange {
                offset: from - first,
                size: to - from,
            }),
        }
    }
    match missing.as_slice() {
        [] => None,
        [whole] if whole.offset == 0 && whole.size == end - first => Some(
            VariableUnavailableReason::OptimizedOut(OptimizedOutReason::EmptyLocation),
        ),
        _ => Some(VariableUnavailableReason::OptimizedOut(
            OptimizedOutReason::UndefinedPieces {
                ranges: missing.into(),
            },
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn optimized_out(offset: u64, size: u64) -> UnavailableBits {
        UnavailableBits {
            range: ValueBitRange { offset, size },
            reason: VariableUnavailableReason::OptimizedOut(OptimizedOutReason::EmptyLocation),
        }
    }

    #[test]
    fn bit_pieces_land_at_their_offsets_from_their_sources_first_bit() {
        let mut value = [0_u8; 3];
        copy_bits(&mut value, 4, &[0b1011_0000, 0b0000_0001], 4, 5);
        copy_bits(&mut value, 16, &[0xab], 0, 8);
        assert_eq!(value, [0b1011_0000, 0b0000_0001, 0xab]);
    }

    #[test]
    fn a_read_reports_only_the_missing_bits_it_covers() {
        let missing = [
            optimized_out(64, 64),
            UnavailableBits {
                range: ValueBitRange {
                    offset: 128,
                    size: 64,
                },
                reason: VariableUnavailableReason::RegisterNotSaved("rsi".into()),
            },
        ];
        assert_eq!(unavailable_within(&missing, 0, 8), None);
        assert_eq!(
            unavailable_within(&missing, 4, 8),
            Some(VariableUnavailableReason::OptimizedOut(
                OptimizedOutReason::UndefinedPieces {
                    ranges: Arc::from([ValueBitRange {
                        offset: 32,
                        size: 32
                    }]),
                }
            ))
        );
        assert_eq!(
            unavailable_within(&missing, 8, 8),
            Some(VariableUnavailableReason::OptimizedOut(
                OptimizedOutReason::EmptyLocation
            ))
        );
        assert_eq!(
            unavailable_within(&missing, 0, 24),
            Some(VariableUnavailableReason::RegisterNotSaved("rsi".into()))
        );
    }
}
