//! Reading attributes from DIEs and the origins they inherit from.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use gimli::Reader as _;

use crate::debug_info::dwarf::{
    DieKey, DwarfError, Reader, die_reference, is_type_unit, source_file_id, source_path,
    string_attribute, type_unit_source_file_id,
};
use crate::{
    AddressRange, ColumnNumber, ImageAddress, LineNumber, SourceFile, SourceFileId, SourceLocation,
    VariableKind,
};

use super::{CatalogDataObject, MAX_DATA_OBJECTS, Scope};

pub(super) const fn check_data_object_capacity(
    count: usize,
) -> std::result::Result<(), DwarfError> {
    if count >= MAX_DATA_OBJECTS {
        return Err(DwarfError::DataObjectLimit(MAX_DATA_OBJECTS));
    }
    Ok(())
}

pub(super) fn variable_order_key(
    object: &CatalogDataObject,
) -> (u8, u8, SourceFileId, u64, u64, u64) {
    if matches!(object.kind, VariableKind::Parameter | VariableKind::Result) {
        return (0, 0, SourceFileId::new(0), 0, 0, object.order);
    }
    object.declaration.as_ref().map_or(
        (
            1,
            1,
            SourceFileId::new(u32::MAX),
            u64::MAX,
            u64::MAX,
            object.order,
        ),
        |location| {
            (
                1,
                0,
                location.file,
                location.line.get(),
                location.column.map_or(0, crate::ColumnNumber::get),
                object.order,
            )
        },
    )
}

pub(super) fn data_object_scope_ranges(
    scope: &Scope,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
) -> (Arc<[AddressRange<ImageAddress>]>, Option<Arc<str>>) {
    let Some(attribute) = entry.attr(gimli::DW_AT_start_scope) else {
        return (Arc::clone(&scope.ranges), None);
    };
    let Some(offset) = attribute.udata_value() else {
        return (
            Arc::clone(&scope.ranges),
            Some("unsupported DW_AT_start_scope form".into()),
        );
    };
    let Some(first) = scope.ranges.first() else {
        return (Arc::clone(&scope.ranges), None);
    };
    let Some(start) = first.start.get().checked_add(offset) else {
        return (
            Arc::clone(&scope.ranges),
            Some("DW_AT_start_scope address overflow".into()),
        );
    };
    let ranges = scope
        .ranges
        .iter()
        .filter_map(|range| {
            let range_start = range.start.get().max(start);
            (range_start < range.end.get()).then_some(AddressRange {
                start: ImageAddress::new(range_start),
                end: range.end,
            })
        })
        .collect::<Vec<_>>()
        .into();
    (ranges, None)
}

pub(super) const fn is_type_scope(tag: gimli::DwTag) -> bool {
    matches!(
        tag,
        gimli::DW_TAG_array_type
            | gimli::DW_TAG_base_type
            | gimli::DW_TAG_class_type
            | gimli::DW_TAG_enumeration_type
            | gimli::DW_TAG_pointer_type
            | gimli::DW_TAG_structure_type
            | gimli::DW_TAG_subroutine_type
            | gimli::DW_TAG_typedef
            | gimli::DW_TAG_union_type
    )
}

pub(super) fn copy_name(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
) -> std::result::Result<Option<Arc<str>>, DwarfError> {
    string_attribute(dwarf, unit, entry, gimli::DW_AT_name)
}

/// Zig's attribute for the type or namespace a declaration is in, which its
/// self-hosted backend writes instead of nesting the declaration's DIE.
pub(super) const DW_AT_ZIG_PARENT: gimli::DwAt = gimli::DwAt(0x2ccd);

/// How many parents a Zig name is qualified through.
const MAX_ZIG_PARENTS: usize = 16;

/// A self-hosted Zig type's name after the names of the parents its
/// `DW_AT_ZIG_parent` chain leads through, as `hash_map.HashMap(…).Header`,
/// the way the LLVM backend spells it. A parent whose own name is
/// qualified, or has no parent, ends the chain.
pub(super) fn zig_qualified_name(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    units: &[gimli::Unit<Reader<'_>>],
    unit_index: usize,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    name: Arc<str>,
) -> std::result::Result<Arc<str>, DwarfError> {
    let mut parts = vec![name];
    let mut visited = HashSet::new();
    let mut current = die_reference(entry.attr_value(DW_AT_ZIG_PARENT), unit_index, units)?;
    // A cycle, or a chain longer than any Zig nests, names nothing; a
    // partial name would be a wrong one.
    while let Some(key) = current {
        if !visited.insert(key) {
            return Err(DwarfError::ReferenceCycle);
        }
        if parts.len() > MAX_ZIG_PARENTS {
            return Err(DwarfError::MalformedVariable(
                "a Zig type's parents nest past their limit".into(),
            ));
        }
        let unit = units
            .get(key.unit)
            .ok_or(DwarfError::ReferenceOutsideUnits(key.offset))?;
        let parent = unit.entry(gimli::UnitOffset(key.offset))?;
        let Some(parent_name) = copy_name(dwarf, unit, &parent)? else {
            break;
        };
        let qualified = parent_name.contains(['.', '(']);
        parts.push(parent_name);
        if qualified {
            break;
        }
        current = die_reference(parent.attr_value(DW_AT_ZIG_PARENT), key.unit, units)?;
    }
    parts.reverse();
    Ok(parts.join(".").into())
}

/// A string attribute of a DIE or, failing that, of the first origin that
/// has it.
pub(super) fn string_with_origins(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    units: &[gimli::Unit<Reader<'_>>],
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    chain: &[(usize, gimli::DebuggingInformationEntry<Reader<'_>>)],
    attribute: gimli::DwAt,
) -> std::result::Result<Option<Arc<str>>, DwarfError> {
    if let Some(value) = string_attribute(dwarf, unit, entry, attribute)? {
        return Ok(Some(value));
    }
    for (origin_unit, origin) in chain {
        if let Some(value) = string_attribute(dwarf, &units[*origin_unit], origin, attribute)? {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

/// The `DW_AT_type` of a DIE or of its first origin that has one, with the
/// index of the unit holding it.
pub(super) fn type_with_origins<'data>(
    unit_index: usize,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
    chain: &[(usize, gimli::DebuggingInformationEntry<Reader<'data>>)],
) -> (usize, Option<gimli::AttributeValue<Reader<'data>>>) {
    entry
        .attr_value(gimli::DW_AT_type)
        .map(|value| (unit_index, Some(value)))
        .or_else(|| {
            chain.iter().find_map(|(origin_unit, origin)| {
                origin
                    .attr_value(gimli::DW_AT_type)
                    .map(|value| (*origin_unit, Some(value)))
            })
        })
        .unwrap_or((unit_index, None))
}

/// A DIE's offset in `.debug_info`, which implicit pointers name it by.
pub(super) fn debug_info_offset(
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
) -> Option<u64> {
    entry
        .offset()
        .to_debug_info_offset(&unit.header)
        .map(|offset| u64::try_from(offset.0).expect("DWARF offset fits u64"))
}

pub(super) fn flag_with_origins(
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    chain: &[(usize, gimli::DebuggingInformationEntry<Reader<'_>>)],
    attribute: gimli::DwAt,
) -> Option<bool> {
    let flag = |attribute: &gimli::Attribute<Reader<'_>>| match attribute.value() {
        gimli::AttributeValue::Flag(value) => Some(value),
        _ => None,
    };
    entry.attr(attribute).and_then(flag).or_else(|| {
        chain
            .iter()
            .find_map(|(_, origin)| origin.attr(attribute).and_then(flag))
    })
}

/// Follows `DW_AT_abstract_origin`/`DW_AT_specification` references
/// transitively, rejecting cycles, so concrete inline-instance DIEs can
/// inherit name, type, and declaration metadata from their origins.
pub(super) fn origin_chain<'data>(
    units: &[gimli::Unit<Reader<'data>>],
    unit_index: usize,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
) -> std::result::Result<Vec<(usize, gimli::DebuggingInformationEntry<Reader<'data>>)>, DwarfError>
{
    let keys = checked_reference_chain(origin_reference(entry, unit_index, units)?, |key| {
        let unit = units
            .get(key.unit)
            .ok_or(DwarfError::ReferenceOutsideUnits(key.offset))?;
        let origin = unit.entry(gimli::UnitOffset(key.offset))?;
        origin_reference(&origin, key.unit, units)
    })?;
    let mut chain = Vec::with_capacity(keys.len());
    for key in keys {
        let unit = units
            .get(key.unit)
            .ok_or(DwarfError::ReferenceOutsideUnits(key.offset))?;
        chain.push((key.unit, unit.entry(gimli::UnitOffset(key.offset))?));
    }
    Ok(chain)
}

pub(super) fn checked_reference_chain(
    mut current: Option<DieKey>,
    mut next: impl FnMut(DieKey) -> std::result::Result<Option<DieKey>, DwarfError>,
) -> std::result::Result<Vec<DieKey>, DwarfError> {
    let mut chain = Vec::new();
    let mut visited = HashSet::new();
    while let Some(key) = current {
        if !visited.insert(key) {
            return Err(DwarfError::ReferenceCycle);
        }
        current = next(key)?;
        chain.push(key);
    }
    Ok(chain)
}

fn origin_reference(
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    unit_index: usize,
    units: &[gimli::Unit<Reader<'_>>],
) -> std::result::Result<Option<DieKey>, DwarfError> {
    let value = entry
        .attr_value(gimli::DW_AT_abstract_origin)
        .or_else(|| entry.attr_value(gimli::DW_AT_specification));
    die_reference(value, unit_index, units)
}

pub(super) fn strict_flag(
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    attribute: gimli::DwAt,
) -> std::result::Result<bool, Arc<str>> {
    match entry.attr_value(attribute) {
        None => Ok(false),
        Some(gimli::AttributeValue::Flag(value)) => Ok(value),
        Some(_) => Err(format!("{attribute:?} has an invalid flag encoding").into()),
    }
}

pub(super) fn declaration_with_origins<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    units: &[gimli::Unit<Reader<'data>>],
    unit: &gimli::Unit<Reader<'data>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
    chain: &[(usize, gimli::DebuggingInformationEntry<Reader<'data>>)],
    source_files: &mut Vec<SourceFile>,
    source_file_ids: &mut HashMap<PathBuf, SourceFileId>,
) -> std::result::Result<Option<SourceLocation>, DwarfError> {
    // DWARF inherits declaration attributes individually: each of decl_file,
    // decl_line, and decl_column comes from the first DIE in the chain that
    // supplies it. decl_file indexes the line program of the unit that owns
    // the DIE supplying it.
    let mut dies = Vec::with_capacity(chain.len() + 1);
    dies.push((unit, entry));
    for (origin_unit, origin_entry) in chain {
        dies.push((&units[*origin_unit], origin_entry));
    }
    let file = dies.iter().find_map(|(unit, entry)| {
        entry
            .attr(gimli::DW_AT_decl_file)
            .and_then(gimli::Attribute::udata_value)
            .map(|index| (*unit, index))
    });
    let line = dies
        .iter()
        .find_map(|(_, entry)| {
            entry
                .attr(gimli::DW_AT_decl_line)
                .and_then(gimli::Attribute::udata_value)
        })
        .and_then(LineNumber::new);
    let (Some((file_unit, file_index)), Some(line)) = (file, line) else {
        return Ok(None);
    };
    let Some(program) = file_unit.line_program.as_ref() else {
        return Ok(None);
    };
    let Some(file) = program.header().file(file_index) else {
        return Ok(None);
    };
    let path = source_path(dwarf, file_unit, program.header(), file)?;
    let file = if is_type_unit(file_unit) {
        type_unit_source_file_id(path, source_files, source_file_ids)
    } else {
        source_file_id(path, source_files, source_file_ids)
    };
    Ok(Some(SourceLocation {
        file,
        line,
        column: dies
            .iter()
            .find_map(|(_, entry)| {
                entry
                    .attr(gimli::DW_AT_decl_column)
                    .and_then(gimli::Attribute::udata_value)
            })
            .and_then(ColumnNumber::new),
    }))
}

/// Returns a member's offset when it is constant: a constant form, or the
/// expression `DW_OP_plus_uconst N` that DWARF 2 and 3 producers emit.
pub(super) fn constant_member_offset(attribute: &gimli::Attribute<Reader<'_>>) -> Option<u64> {
    if let Some(offset) = attribute.udata_value() {
        return Some(offset);
    }
    let mut bytes = attribute.exprloc_value()?.0;
    if bytes.read_u8().ok()? != gimli::DW_OP_plus_uconst.0 {
        return None;
    }
    let offset = bytes.read_uleb128().ok()?;
    bytes.is_empty().then_some(offset)
}

/// Decodes a constant array bound. Fixed-size data forms carry no sign of
/// their own and follow the subrange's index type, which is unsigned for C
/// arrays: GCC writes the upper bound of `char[256]` as the byte 0xff.
pub(super) fn array_bound(
    attribute: &gimli::Attribute<Reader<'_>>,
    signed_index: bool,
) -> Option<i128> {
    match attribute.value() {
        gimli::AttributeValue::Sdata(value) => Some(value.into()),
        gimli::AttributeValue::Udata(value) => Some(value.into()),
        gimli::AttributeValue::Data1(_)
        | gimli::AttributeValue::Data2(_)
        | gimli::AttributeValue::Data4(_)
        | gimli::AttributeValue::Data8(_) => {
            if signed_index {
                attribute.sdata_value().map(i128::from)
            } else {
                attribute.udata_value().map(i128::from)
            }
        }
        // A reference or expression describes a bound known only at run time.
        _ => None,
    }
}

/// Whether a subrange's index type is a signed integer.
pub(super) fn index_type_is_signed(
    unit: &gimli::Unit<Reader<'_>>,
    subrange: &gimli::DebuggingInformationEntry<Reader<'_>>,
) -> bool {
    let Some(gimli::AttributeValue::UnitRef(offset)) = subrange.attr_value(gimli::DW_AT_type)
    else {
        return false;
    };
    unit.entry(offset).is_ok_and(|index_type| {
        matches!(
            index_type.attr_value(gimli::DW_AT_encoding),
            Some(gimli::AttributeValue::Encoding(
                gimli::DW_ATE_signed | gimli::DW_ATE_signed_char
            ))
        )
    })
}

/// The classified form of a `DW_AT_byte_size` attribute.
pub(super) enum ByteSize {
    /// The attribute is not present; a default size may apply.
    Absent,
    /// A constant unsigned size in bytes.
    Constant(u64),
    /// A valid constant above `u64::MAX`.
    Unsupported(Arc<str>),
    /// A location expression or DIE reference, known only at run time.
    Dynamic,
    /// A form DWARF does not permit for a byte size.
    Malformed,
}

/// The classified form of an attribute expected to hold an unsigned integer
/// constant.
pub(super) enum UnsignedConstant {
    /// A representable constant value.
    Value(u64),
    /// A valid `DW_FORM_data16` constant above `u64::MAX`.
    Oversized,
    /// A form that is not an integer constant.
    NonConstant,
}

/// Classifies an attribute expected to be an unsigned integer constant,
/// including `DW_FORM_data16`, which `udata_value` does not decode.
pub(super) fn unsigned_constant(attribute: &gimli::Attribute<Reader<'_>>) -> UnsignedConstant {
    if let Some(value) = attribute.udata_value() {
        return UnsignedConstant::Value(value);
    }
    match attribute.value() {
        gimli::AttributeValue::Data16(value) => {
            u64::try_from(value).map_or(UnsignedConstant::Oversized, UnsignedConstant::Value)
        }
        _ => UnsignedConstant::NonConstant,
    }
}

/// Extracts a base type's `DW_AT_encoding` as a one-byte `DW_ATE_*` value;
/// anything else is malformed.
pub(super) fn base_type_encoding(
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
) -> std::result::Result<u8, Arc<str>> {
    let Some(attribute) = entry.attr(gimli::DW_AT_encoding) else {
        return Err("base type has no encoding".into());
    };
    match unsigned_constant(attribute) {
        UnsignedConstant::Value(value) => u8::try_from(value)
            .map_err(|_| Arc::from("DW_AT_encoding exceeds the one-byte DW_ATE domain")),
        UnsignedConstant::Oversized => {
            Err("DW_AT_encoding exceeds the one-byte DW_ATE domain".into())
        }
        UnsignedConstant::NonConstant => {
            Err("DW_AT_encoding is not an unsigned integer constant".into())
        }
    }
}

pub(super) fn byte_size_attribute(
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
) -> ByteSize {
    let Some(attribute) = entry.attr(gimli::DW_AT_byte_size) else {
        return ByteSize::Absent;
    };
    match unsigned_constant(attribute) {
        UnsignedConstant::Value(size) => ByteSize::Constant(size),
        UnsignedConstant::Oversized => {
            ByteSize::Unsupported("constant DW_AT_byte_size exceeds the supported u64 range".into())
        }
        // DWARF also permits an expression or a DIE reference, which are
        // valid but not statically sizable; any other form is malformed.
        UnsignedConstant::NonConstant => match attribute.value() {
            gimli::AttributeValue::Exprloc(_)
            | gimli::AttributeValue::Block(_)
            | gimli::AttributeValue::UnitRef(_)
            | gimli::AttributeValue::DebugInfoRef(_)
            | gimli::AttributeValue::DebugInfoRefSup(_)
            | gimli::AttributeValue::DebugTypesRef(_) => ByteSize::Dynamic,
            _ => ByteSize::Malformed,
        },
    }
}
