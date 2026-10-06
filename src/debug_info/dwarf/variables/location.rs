//! Location descriptions and DWARF expressions copied out of the debug sections.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use gimli::Reader as _;

use crate::debug_info::dwarf::{DwarfError, Reader};
use crate::{AddressRange, ImageAddress, VariableUnavailableReason};

use super::die::{ByteSize, base_type_encoding, byte_size_attribute};
use super::{ConstantValue, Metadata, MetadataAbsence, ValueDescription};

/// How many procedures one expression may call, directly or not.
const MAX_PROCEDURES: usize = 64;

#[derive(Clone)]
pub(super) struct Expression {
    pub(super) bytes: Arc<[u8]>,
    pub(super) encoding: gimli::Encoding,
    pub(super) unit: usize,
    pub(super) indexed_addresses: Arc<HashMap<usize, u64>>,
    /// The locations of the entries `DW_OP_call2`, `DW_OP_call4`, and
    /// `DW_OP_call_ref` run, directly or from another such procedure, by
    /// `.debug_info` offset: `None` for an entry without a location.
    pub(super) procedures: Arc<HashMap<u64, Option<LocationDescription>>>,
}

/// Operation families that determine where an object's storage lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ExpressionUse {
    ThreadLocal,
    Dereference,
    Frame,
    RegisterValue,
    Computed,
}

pub(super) struct EvaluationUnit {
    pub(super) base_types: HashMap<usize, gimli::ValueType>,
    pub(super) language: Option<gimli::DwLang>,
    /// Where the unit begins in `.debug_info`, which its unit-relative
    /// references are offsets from.
    pub(super) offset: Option<u64>,
}

#[derive(Clone)]
pub(super) struct LocationEntry {
    pub(super) range: Option<AddressRange<ImageAddress>>,
    pub(super) expression: Expression,
}

#[derive(Clone)]
pub(super) struct LocationDescription {
    pub(super) entries: Arc<[LocationEntry]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LocationSelectionError {
    Unavailable(VariableUnavailableReason),
    Malformed(Arc<str>),
}

impl LocationDescription {
    pub(super) fn expression(
        &self,
        address: Option<ImageAddress>,
    ) -> std::result::Result<Option<&Expression>, LocationSelectionError> {
        // An entry whose range holds the address overrides a default
        // (range-less) entry; without an address only a default can apply.
        if let Some(address) = address {
            let mut specific = self
                .entries
                .iter()
                .filter(|entry| entry.range.is_some_and(|range| range.contains(address)));
            if let Some(entry) = specific.next() {
                if specific.next().is_some() {
                    return Err(LocationSelectionError::Unavailable(
                        crate::UnsupportedVariableFeature::AlternativeLocations.into(),
                    ));
                }
                return Ok(Some(&entry.expression));
            }
        }
        let mut defaults = self.entries.iter().filter(|entry| entry.range.is_none());
        let expression = defaults.next().map(|entry| &entry.expression);
        if defaults.next().is_some() {
            return Err(LocationSelectionError::Malformed(
                "multiple default locations were supplied".into(),
            ));
        }
        if address.is_none() && expression.is_none() && !self.entries.is_empty() {
            return Err(LocationSelectionError::Unavailable(
                VariableUnavailableReason::NoInstructionContext,
            ));
        }
        Ok(expression)
    }
}

pub(super) fn copy_optional_location(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    value: Option<gimli::AttributeValue<Reader<'_>>>,
    absence: MetadataAbsence,
) -> Metadata<LocationDescription> {
    let Some(value) = value else {
        return Metadata::Absent(absence);
    };
    match copy_location(dwarf, unit_index, unit, value) {
        Ok(location) => Metadata::Value(location),
        Err(error) => Metadata::Malformed(error.to_string().into()),
    }
}

pub(super) fn copy_data_object_value(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
) -> Metadata<ValueDescription> {
    if let Some(location) = entry.attr_value(gimli::DW_AT_location) {
        return match copy_location(dwarf, unit_index, unit, location) {
            Ok(location) => Metadata::Value(ValueDescription::Location(location)),
            Err(error) => Metadata::Malformed(error.to_string().into()),
        };
    }
    if let Some(value) = entry.attr_value(gimli::DW_AT_const_value) {
        return match copy_constant(value) {
            Ok(value) => Metadata::Value(ValueDescription::Constant(value)),
            Err(error) => Metadata::Malformed(error),
        };
    }
    Metadata::Absent(MetadataAbsence::NoLocation)
}

pub(super) fn copy_data_object_value_with_origins(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    units: &[gimli::Unit<Reader<'_>>],
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    chain: &[(usize, gimli::DebuggingInformationEntry<Reader<'_>>)],
) -> Metadata<ValueDescription> {
    std::iter::once(copy_data_object_value(dwarf, unit_index, unit, entry))
        .chain(chain.iter().map(|(origin_unit, origin)| {
            copy_data_object_value(dwarf, *origin_unit, &units[*origin_unit], origin)
        }))
        .find(|value| !matches!(value, Metadata::Absent(_)))
        .unwrap_or(Metadata::Absent(MetadataAbsence::NoLocation))
}

fn copy_constant(
    value: gimli::AttributeValue<Reader<'_>>,
) -> std::result::Result<ConstantValue, Arc<str>> {
    Ok(match value {
        // Fixed-width forms carry raw bits; signedness comes from the type.
        gimli::AttributeValue::Data1(value) => ConstantValue::Fixed(u128::from(value)),
        gimli::AttributeValue::Data2(value) => ConstantValue::Fixed(u128::from(value)),
        gimli::AttributeValue::Data4(value) => ConstantValue::Fixed(u128::from(value)),
        gimli::AttributeValue::Data8(value) => ConstantValue::Fixed(u128::from(value)),
        gimli::AttributeValue::Data16(value) => ConstantValue::Fixed(value),
        gimli::AttributeValue::Udata(value) => ConstantValue::Unsigned(u128::from(value)),
        gimli::AttributeValue::Sdata(value) => ConstantValue::Signed(i128::from(value)),
        gimli::AttributeValue::Block(value) => ConstantValue::Bytes(Arc::from(
            value
                .to_slice()
                .map_err(|error| Arc::from(error.to_string()))?
                .into_owned(),
        )),
        _ => return Err("unsupported DW_AT_const_value form".into()),
    })
}

fn copy_location(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    value: gimli::AttributeValue<Reader<'_>>,
) -> std::result::Result<LocationDescription, DwarfError> {
    copy_location_with(dwarf, unit_index, unit, value, copy_expression)
}

type CopyExpression = fn(
    &gimli::Dwarf<Reader<'_>>,
    usize,
    &gimli::Unit<Reader<'_>>,
    gimli::Expression<Reader<'_>>,
    gimli::Encoding,
) -> std::result::Result<Expression, DwarfError>;

fn copy_location_with(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    value: gimli::AttributeValue<Reader<'_>>,
    copy_expression: CopyExpression,
) -> std::result::Result<LocationDescription, DwarfError> {
    let encoding = unit.encoding();
    // DWARF 2 and 3 encode a single expression as a block.
    if let Some(expression) = value.exprloc_value() {
        return Ok(LocationDescription {
            entries: vec![LocationEntry {
                range: None,
                expression: copy_expression(dwarf, unit_index, unit, expression, encoding)?,
            }]
            .into(),
        });
    }
    let mut locations = dwarf
        .attr_locations(unit, value)?
        .ok_or(DwarfError::UnsupportedReferenceForm)?;
    let mut entries = Vec::new();
    // Iterate raw entries so DW_LLE_default_location keeps its fallback
    // semantics (range: None) instead of becoming a 0..u64::MAX range that
    // conflicts with every specific entry.
    while let Some(raw) = locations.next_raw()? {
        let is_default = matches!(&raw, gimli::RawLocListEntry::DefaultLocation { .. });
        let Some(location) = locations.convert_raw(raw)? else {
            continue;
        };
        if is_default {
            entries.push(LocationEntry {
                range: None,
                expression: copy_expression(dwarf, unit_index, unit, location.data, encoding)?,
            });
            continue;
        }
        if location.range.begin > location.range.end {
            return Err(DwarfError::InvalidRange);
        }
        if location.range.begin < location.range.end {
            entries.push(LocationEntry {
                range: Some(AddressRange {
                    start: ImageAddress::new(location.range.begin),
                    end: ImageAddress::new(location.range.end),
                }),
                expression: copy_expression(dwarf, unit_index, unit, location.data, encoding)?,
            });
        }
    }
    Ok(LocationDescription {
        entries: entries.into(),
    })
}

/// Copies an expression with the procedures it calls, which must be in its
/// own unit.
pub(super) fn copy_expression(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    expression: gimli::Expression<Reader<'_>>,
    encoding: gimli::Encoding,
) -> std::result::Result<Expression, DwarfError> {
    let endian = expression.0.endian();
    let copied = copy_operations(dwarf, unit_index, unit, expression, encoding)?;
    let mut pending = calls(unit, &copied, endian)?;
    if pending.is_empty() {
        return Ok(copied);
    }
    let mut procedures = HashMap::new();
    while let Some(offset) = pending.pop() {
        if procedures.contains_key(&offset) {
            continue;
        }
        if procedures.len() == MAX_PROCEDURES {
            return Err(DwarfError::MalformedVariable(
                "an expression calls too many procedures".into(),
            ));
        }
        // A procedure elsewhere stays missing, and running it unsupported.
        let Some(entry_offset) =
            gimli::DebugInfoOffset(usize::try_from(offset).map_err(|_| DwarfError::InvalidRange)?)
                .to_unit_offset(&unit.header)
        else {
            continue;
        };
        let entry = unit.entry(entry_offset)?;
        let location = entry
            .attr_value(gimli::DW_AT_location)
            .map(|value| copy_location_with(dwarf, unit_index, unit, value, copy_operations))
            .transpose()?;
        for called in location.iter().flat_map(|location| location.entries.iter()) {
            pending.extend(calls(unit, &called.expression, endian)?);
        }
        procedures.insert(offset, location);
    }
    Ok(with_procedures(copied, procedures))
}

/// Gives `expression` the procedures it calls. They run on its evaluation,
/// which resolves their indexed addresses from its table; all are in one
/// unit, whose table they share.
#[expect(
    clippy::disallowed_methods,
    reason = "one unit maps each index to one address, so any order fills the table alike"
)]
pub(super) fn with_procedures(
    mut expression: Expression,
    procedures: HashMap<u64, Option<LocationDescription>>,
) -> Expression {
    let mut indexed_addresses = (*expression.indexed_addresses).clone();
    for called in procedures
        .values()
        .flatten()
        .flat_map(|location| location.entries.iter())
    {
        indexed_addresses.extend(called.expression.indexed_addresses.iter());
    }
    expression.indexed_addresses = Arc::new(indexed_addresses);
    expression.procedures = Arc::new(procedures);
    expression
}

/// Copies an expression's operations without the procedures it calls.
fn copy_operations(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    expression: gimli::Expression<Reader<'_>>,
    encoding: gimli::Encoding,
) -> std::result::Result<Expression, DwarfError> {
    let mut indexed_addresses = HashMap::new();
    let mut operations = expression.operations(encoding);
    while let Some(operation) = operations.next()? {
        let (gimli::Operation::AddressIndex { index } | gimli::Operation::ConstantIndex { index }) =
            operation
        else {
            continue;
        };
        let address = dwarf.address(unit, index)?;
        indexed_addresses.insert(index.0, address);
    }
    let bytes: Cow<'_, [u8]> = expression.0.to_slice()?;
    Ok(Expression {
        bytes: Arc::from(bytes.into_owned()),
        encoding,
        unit: unit_index,
        indexed_addresses: Arc::new(indexed_addresses),
        procedures: Arc::default(),
    })
}

/// The `.debug_info` offsets of the entries an expression calls.
fn calls(
    unit: &gimli::Unit<Reader<'_>>,
    expression: &Expression,
    endian: gimli::RunTimeEndian,
) -> std::result::Result<Vec<u64>, DwarfError> {
    let reader = gimli::EndianSlice::new(&expression.bytes, endian);
    let mut operations = gimli::Expression(reader).operations(expression.encoding);
    let mut called = Vec::new();
    while let Some(operation) = operations.next()? {
        let gimli::Operation::Call { offset } = operation else {
            continue;
        };
        let offset = match offset {
            gimli::DieReference::UnitRef(offset) => offset.to_debug_info_offset(&unit.header),
            gimli::DieReference::DebugInfoRef(offset) => Some(offset),
        };
        if let Some(offset) = offset {
            called.push(u64::try_from(offset.0).expect("DWARF offset fits u64"));
        }
    }
    Ok(called)
}

pub(super) fn load_evaluation_units(
    units: &[gimli::Unit<Reader<'_>>],
) -> std::result::Result<Vec<EvaluationUnit>, DwarfError> {
    units
        .iter()
        .map(|unit| {
            let mut base_types = HashMap::new();
            let mut language = None;
            let mut entries = unit.entries();
            while let Some(entry) = entries.next_dfs()? {
                if entry.tag() == gimli::DW_TAG_compile_unit {
                    if let Some(gimli::AttributeValue::Language(value)) =
                        entry.attr_value(gimli::DW_AT_language)
                    {
                        language = Some(value);
                    }
                    continue;
                }
                if entry.tag() != gimli::DW_TAG_base_type {
                    continue;
                }
                let ByteSize::Constant(byte_size) = byte_size_attribute(entry) else {
                    continue;
                };
                let Ok(raw_encoding) = base_type_encoding(entry) else {
                    continue;
                };
                let encoding = gimli::DwAte(raw_encoding);
                if let Some(value_type) = dwarf_value_type(encoding, byte_size) {
                    base_types.insert(entry.offset().0, value_type);
                }
            }
            Ok(EvaluationUnit {
                base_types,
                language,
                offset: unit
                    .header
                    .debug_info_offset()
                    .map(|offset| u64::try_from(offset.0).expect("DWARF offset fits u64")),
            })
        })
        .collect()
}

const fn dwarf_value_type(encoding: gimli::DwAte, byte_size: u64) -> Option<gimli::ValueType> {
    use gimli::ValueType::{F32, F64, I8, I16, I32, I64, U8, U16, U32, U64};
    let signed = matches!(encoding, gimli::DW_ATE_signed | gimli::DW_ATE_signed_char);
    let unsigned = matches!(
        encoding,
        gimli::DW_ATE_boolean | gimli::DW_ATE_unsigned | gimli::DW_ATE_unsigned_char
    );
    Some(match (encoding, signed, unsigned, byte_size) {
        (gimli::DW_ATE_float, _, _, 4) => F32,
        (gimli::DW_ATE_float, _, _, 8) => F64,
        (_, true, _, 1) => I8,
        (_, true, _, 2) => I16,
        (_, true, _, 4) => I32,
        (_, true, _, 8) => I64,
        (_, _, true, 1) => U8,
        (_, _, true, 2) => U16,
        (_, _, true, 4) => U32,
        (_, _, true, 8) => U64,
        _ => return None,
    })
}
