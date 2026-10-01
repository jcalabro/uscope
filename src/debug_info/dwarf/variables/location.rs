//! Location descriptions and DWARF expressions copied out of the debug sections.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use gimli::Reader as _;

use crate::debug_info::dwarf::{DwarfError, Reader};
use crate::{AddressRange, ImageAddress, VariableUnavailableReason};

use super::die::{ByteSize, base_type_encoding, byte_size_attribute};
use super::{ConstantValue, Metadata, MetadataAbsence, ValueDescription};

#[derive(Clone)]
pub(super) struct Expression {
    pub(super) bytes: Arc<[u8]>,
    pub(super) encoding: gimli::Encoding,
    pub(super) unit: usize,
    pub(super) indexed_addresses: Arc<HashMap<usize, u64>>,
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
        // Specific ranged entries override default (range-less) entries per
        // DWARF 5 default-location semantics. Without an instruction context we
        // cannot select a ranged entry; a range-less default still resolves, but
        // an entry that only exists behind a range must fail explicitly rather
        // than silently resolve against a guessed address.
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
            // A global was requested without a valid module-relative instruction,
            // yet every location entry is range-gated. Refuse to guess.
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
        return match copy_optional_location(
            dwarf,
            unit_index,
            unit,
            Some(location),
            MetadataAbsence::NoLocation,
        ) {
            Metadata::Value(location) => Metadata::Value(ValueDescription::Location(location)),
            Metadata::Absent(reason) => Metadata::Absent(reason),
            Metadata::Malformed(reason) => Metadata::Malformed(reason),
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
    let direct = copy_data_object_value(dwarf, unit_index, unit, entry);
    if !matches!(direct, Metadata::Absent(_)) {
        return direct;
    }
    for (origin_unit, origin) in chain {
        let inherited = copy_data_object_value(dwarf, *origin_unit, &units[*origin_unit], origin);
        if !matches!(inherited, Metadata::Absent(_)) {
            return inherited;
        }
    }
    direct
}

pub(super) fn copy_constant(
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

pub(super) fn copy_location(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    value: gimli::AttributeValue<Reader<'_>>,
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

pub(super) fn copy_expression(
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
    })
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
                // Use the shared constant/encoding classifiers so a base type
                // encoded with `DW_FORM_data16` is recognized here too. Any form
                // this backend cannot use is simply skipped for typed evaluation.
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
            })
        })
        .collect()
}

pub(super) const fn dwarf_value_type(
    encoding: gimli::DwAte,
    byte_size: u64,
) -> Option<gimli::ValueType> {
    use gimli::ValueType::{F32, F64, I8, I16, I32, I64, U8, U16, U32, U64};
    if encoding.0 == gimli::DW_ATE_float.0 {
        return match byte_size {
            4 => Some(F32),
            8 => Some(F64),
            _ => None,
        };
    }
    let signed = encoding.0 == gimli::DW_ATE_signed.0 || encoding.0 == gimli::DW_ATE_signed_char.0;
    let unsigned = encoding.0 == gimli::DW_ATE_boolean.0
        || encoding.0 == gimli::DW_ATE_unsigned.0
        || encoding.0 == gimli::DW_ATE_unsigned_char.0;
    match (signed, unsigned, byte_size) {
        (true, false, 1) => Some(I8),
        (true, false, 2) => Some(I16),
        (true, false, 4) => Some(I32),
        (true, false, 8) => Some(I64),
        (false, true, 1) => Some(U8),
        (false, true, 2) => Some(U16),
        (false, true, 4) => Some(U32),
        (false, true, 8) => Some(U64),
        _ => None,
    }
}
