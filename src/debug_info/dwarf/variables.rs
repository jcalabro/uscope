use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use gimli::{EvaluationResult, Location, Reader as _, RunTimeEndian, Value};

use super::{DieKey, DwarfError, Reader, die_reference, source_file_id, source_path};
use crate::debug_info::{VariableInfo, VariableRuntime};
use crate::{
    AddressRange, Architecture, BaseType, BaseTypeEncoding, ByteOrder, CodeInstanceId,
    ColumnNumber, Error, FloatValue, ImageAddress, LineNumber, Result, ScalarValue, SourceFile,
    SourceFileId, SourceLocation, TargetDescription, Variable, VariableKind,
    VariableMalformedReason, VariableQuery, VariableState, VariableUnavailableReason,
    VariableValueSource, VirtualAddress,
};

const MAX_SCALAR_BYTES: u64 = 16;
const MAX_EVALUATION_ITERATIONS: u32 = 10_000;
const MAX_EVALUATION_MEMORY_READS: u32 = 64;
const MAX_EVALUATION_MEMORY_BYTES: usize = 1_024;
const MAX_LOCATION_PIECES: usize = 64;

#[derive(Default)]
struct EvaluationBudget {
    memory_reads: u32,
    memory_bytes: usize,
}

impl EvaluationBudget {
    fn consume_memory(
        &mut self,
        size: usize,
    ) -> std::result::Result<(), VariableUnavailableReason> {
        self.memory_reads = self
            .memory_reads
            .checked_add(1)
            .ok_or(VariableUnavailableReason::EvaluationLimit)?;
        self.memory_bytes = self
            .memory_bytes
            .checked_add(size)
            .ok_or(VariableUnavailableReason::EvaluationLimit)?;
        if self.memory_reads > MAX_EVALUATION_MEMORY_READS
            || self.memory_bytes > MAX_EVALUATION_MEMORY_BYTES
        {
            return Err(VariableUnavailableReason::EvaluationLimit);
        }
        Ok(())
    }
}

enum FrameBaseCache {
    Empty,
    Available(VirtualAddress),
    Unavailable(VariableUnavailableReason),
    Malformed(Arc<str>),
}

#[derive(Clone)]
struct Expression {
    bytes: Arc<[u8]>,
    encoding: gimli::Encoding,
    unit: usize,
    indexed_addresses: Arc<HashMap<usize, u64>>,
    requires_frame_base: bool,
}

struct EvaluationUnit {
    base_types: HashMap<usize, gimli::ValueType>,
}

#[derive(Clone)]
struct LocationEntry {
    range: Option<AddressRange<ImageAddress>>,
    expression: Expression,
}

#[derive(Clone)]
struct LocationDescription {
    entries: Arc<[LocationEntry]>,
}

impl LocationDescription {
    fn expression(
        &self,
        address: ImageAddress,
    ) -> std::result::Result<Option<&Expression>, VariableUnavailableReason> {
        // Specific ranged entries override default (range-less) entries per
        // DWARF 5 default-location semantics.
        let mut specific = self
            .entries
            .iter()
            .filter(|entry| entry.range.is_some_and(|range| range.contains(address)));
        if let Some(entry) = specific.next() {
            if specific.next().is_some() {
                return Err("multiple locations are active at the current instruction".into());
            }
            return Ok(Some(&entry.expression));
        }
        let mut defaults = self.entries.iter().filter(|entry| entry.range.is_none());
        let expression = defaults.next().map(|entry| &entry.expression);
        if defaults.next().is_some() {
            return Err("multiple default locations were supplied".into());
        }
        Ok(expression)
    }
}

#[derive(Clone)]
enum TypeResolution {
    Scalar(BaseType),
    Unsupported(Arc<str>),
    Malformed(Arc<str>),
}

#[derive(Clone)]
enum Metadata<T> {
    Value(T),
    Unavailable(Arc<str>),
    Malformed(Arc<str>),
}

#[derive(Clone)]
enum ConstantValue {
    Unsigned(u128),
    Signed(i128),
    Bytes(Arc<[u8]>),
}

#[derive(Clone)]
enum ValueDescription {
    Location(LocationDescription),
    Constant(ConstantValue),
}

#[derive(Clone)]
struct CatalogDataObject {
    kind: VariableKind,
    name: Arc<str>,
    declaration: Option<SourceLocation>,
    ranges: Arc<[AddressRange<ImageAddress>]>,
    /// The inline instance owning this variable, or `None` for the physical
    /// frame. Lookup only sees variables of the selected logical frame.
    instance: Option<CodeInstanceId>,
    lexical_depth: u32,
    order: u64,
    type_info: TypeResolution,
    value: Metadata<ValueDescription>,
    frame_base: Metadata<LocationDescription>,
    malformed: Option<Arc<str>>,
}

#[derive(Clone)]
struct Scope {
    ranges: Arc<[AddressRange<ImageAddress>]>,
    lexical_depth: u32,
    frame_base: Metadata<LocationDescription>,
    /// True for subprograms and inlined subroutines, whose direct children
    /// may include formal parameters.
    routine: bool,
    function: usize,
    /// The innermost containing inline instance, or `None` when the scope
    /// belongs directly to the physical frame.
    instance: Option<CodeInstanceId>,
    malformed: Option<Arc<str>>,
}

struct CatalogFunction {
    ranges: Arc<[AddressRange<ImageAddress>]>,
    objects: Vec<usize>,
}

pub(super) struct DwarfVariableInfo {
    objects: Arc<[CatalogDataObject]>,
    functions: Arc<[CatalogFunction]>,
    address_index: BTreeMap<ImageAddress, Arc<[usize]>>,
    evaluation_units: Arc<[EvaluationUnit]>,
    target: TargetDescription,
    endian: RunTimeEndian,
}

#[expect(
    clippy::too_many_lines,
    reason = "one depth-first DIE walk must keep scope, variable, and parameter state synchronized"
)]
pub(super) fn load_variable_info(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    units: &[gimli::Unit<Reader<'_>>],
    target: TargetDescription,
    instance_ids: &HashMap<DieKey, CodeInstanceId>,
    source_files: &mut Vec<SourceFile>,
    source_file_ids: &mut HashMap<PathBuf, SourceFileId>,
) -> std::result::Result<Arc<dyn VariableInfo>, DwarfError> {
    let mut objects = Vec::new();
    let mut functions = Vec::new();
    let mut order = 0_u64;
    let evaluation_units = load_evaluation_units(units)?;

    for (unit_index, unit) in units.iter().enumerate() {
        let mut entries = unit.entries();
        let mut scopes = Vec::<Option<Scope>>::new();

        while let Some(entry) = entries.next_dfs()? {
            let depth =
                usize::try_from(entry.depth()).map_err(|_| DwarfError::InvalidEntryDepth)?;
            scopes.truncate(depth);
            let parent = scopes.last().and_then(Clone::clone);

            let scope = match entry.tag() {
                gimli::DW_TAG_subprogram => {
                    let ranges = copy_ranges(dwarf, unit, entry)?;
                    let function = functions.len();
                    functions.push(CatalogFunction {
                        ranges: Arc::clone(&ranges),
                        objects: Vec::new(),
                    });
                    Some(Scope {
                        ranges,
                        lexical_depth: 0,
                        frame_base: copy_optional_location(
                            dwarf,
                            unit_index,
                            unit,
                            entry.attr_value(gimli::DW_AT_frame_base),
                        ),
                        routine: true,
                        function,
                        instance: None,
                        malformed: None,
                    })
                }
                gimli::DW_TAG_lexical_block => parent.as_ref().map(|parent| {
                    let (ranges, malformed) = match copy_ranges(dwarf, unit, entry) {
                        Ok(ranges) if !ranges.is_empty() => (ranges, None),
                        Ok(_) => (Arc::clone(&parent.ranges), None),
                        Err(error) => (Arc::clone(&parent.ranges), Some(error.to_string().into())),
                    };
                    Scope {
                        ranges,
                        lexical_depth: parent.lexical_depth.saturating_add(1),
                        frame_base: parent.frame_base.clone(),
                        routine: false,
                        function: parent.function,
                        instance: parent.instance,
                        malformed: malformed.or_else(|| parent.malformed.clone()),
                    }
                }),
                // An inline instance keeps the caller's frame base and function
                // while narrowing to its own code ranges. Unlike a lexical
                // block, an instance with no usable ranges must not widen to
                // the caller's extent: give it an empty extent so its locals
                // and parameters can never contaminate lookups.
                gimli::DW_TAG_inlined_subroutine => parent.as_ref().map(|parent| {
                    let instance = instance_ids
                        .get(&DieKey {
                            unit: unit_index,
                            offset: entry.offset().0,
                        })
                        .copied();
                    let (ranges, malformed) = match copy_ranges(dwarf, unit, entry) {
                        Ok(ranges) if ranges.is_empty() => (
                            Vec::new().into(),
                            Some(Arc::from("inlined subroutine has no address ranges")),
                        ),
                        // A ranged instance must be identified so lookups can
                        // scope to it; without an identity its contents could
                        // only be misattributed.
                        Ok(_) if instance.is_none() => (
                            Vec::new().into(),
                            Some(Arc::from("inlined subroutine has no code instance")),
                        ),
                        Ok(ranges) => (ranges, None),
                        Err(error) => (Vec::new().into(), Some(error.to_string().into())),
                    };
                    Scope {
                        ranges,
                        lexical_depth: parent.lexical_depth.saturating_add(1),
                        frame_base: parent.frame_base.clone(),
                        routine: true,
                        function: parent.function,
                        instance,
                        malformed: malformed.or_else(|| parent.malformed.clone()),
                    }
                }),
                tag if is_type_scope(tag) => None,
                _ => parent.clone(),
            };
            // An empty extent is deliberate containment (a rangeless inline
            // instance) and must stay empty through every descendant scope;
            // only a nested subprogram starts an independent extent.
            let scope = if entry.tag() != gimli::DW_TAG_subprogram
                && parent
                    .as_ref()
                    .is_some_and(|parent| parent.ranges.is_empty())
            {
                scope.map(|mut scope| {
                    scope.ranges = Vec::new().into();
                    scope
                })
            } else {
                scope
            };

            let kind = match entry.tag() {
                gimli::DW_TAG_variable => Some(VariableKind::Local),
                gimli::DW_TAG_formal_parameter => Some(VariableKind::Parameter),
                _ => None,
            };
            if let Some(kind) = kind {
                let owning_scope = parent.as_ref().filter(|scope| {
                    !scope.ranges.is_empty() && (kind == VariableKind::Local || scope.routine)
                });
                if let Some(scope) = owning_scope {
                    // Concrete inline-instance entries reference their
                    // abstract origin for descriptive metadata.
                    let (chain, chain_error) = match origin_chain(units, unit_index, entry) {
                        Ok(chain) => (chain, None),
                        Err(error) => (Vec::new(), Some(Arc::from(error.to_string()))),
                    };
                    let object_name = match kind {
                        VariableKind::Parameter => "parameter",
                        VariableKind::Local => "variable",
                    };
                    let (name, name_error) =
                        match copy_name_with_origins(dwarf, units, unit, entry, &chain) {
                            Ok(Some(name)) => (name, None),
                            Ok(None) => (
                                format!("<anonymous {object_name} at {:#x}>", entry.offset().0)
                                    .into(),
                                Some(Arc::from(format!("{object_name} has no name"))),
                            ),
                            Err(error) => (
                                format!("<malformed {object_name} at {:#x}>", entry.offset().0)
                                    .into(),
                                Some(error.to_string().into()),
                            ),
                        };
                    order = order
                        .checked_add(1)
                        .expect("data-object DIE order overflow");
                    let declaration = declaration_with_origins(
                        dwarf,
                        units,
                        unit,
                        entry,
                        &chain,
                        source_files,
                        source_file_ids,
                    );
                    let (ranges, scope_error) = data_object_scope_ranges(scope, entry);
                    let (type_unit, type_value) = entry
                        .attr_value(gimli::DW_AT_type)
                        .map(|value| (unit_index, Some(value)))
                        .or_else(|| {
                            chain.iter().find_map(|(origin_unit, origin_entry)| {
                                origin_entry
                                    .attr_value(gimli::DW_AT_type)
                                    .map(|value| (*origin_unit, Some(value)))
                            })
                        })
                        .unwrap_or((unit_index, None));
                    functions[scope.function].objects.push(objects.len());
                    objects.push(CatalogDataObject {
                        kind,
                        name,
                        declaration: declaration.as_ref().ok().cloned().flatten(),
                        ranges,
                        instance: scope.instance,
                        lexical_depth: scope.lexical_depth,
                        order,
                        type_info: resolve_variable_type(dwarf, units, type_unit, type_value),
                        value: copy_data_object_value(dwarf, unit_index, unit, entry),
                        frame_base: scope.frame_base.clone(),
                        malformed: declaration
                            .err()
                            .map(|error| error.to_string().into())
                            .or(scope_error)
                            .or_else(|| scope.malformed.clone())
                            .or(chain_error)
                            .or(name_error),
                    });
                }
            }

            scopes.push(scope);
        }
    }

    let mut address_index = BTreeMap::<ImageAddress, Vec<usize>>::new();
    for (function, metadata) in functions.iter().enumerate() {
        for range in metadata.ranges.iter() {
            address_index.entry(range.start).or_default().push(function);
        }
    }
    Ok(Arc::new(DwarfVariableInfo {
        objects: objects.into(),
        functions: functions.into(),
        address_index: address_index
            .into_iter()
            .map(|(address, functions)| (address, functions.into()))
            .collect(),
        evaluation_units: evaluation_units.into(),
        target,
        endian: match target.byte_order {
            ByteOrder::Little => RunTimeEndian::Little,
            ByteOrder::Big => RunTimeEndian::Big,
        },
    }))
}

fn data_object_scope_ranges(
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

const fn is_type_scope(tag: gimli::DwTag) -> bool {
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

fn copy_name(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
) -> std::result::Result<Option<Arc<str>>, DwarfError> {
    entry
        .attr_value(gimli::DW_AT_name)
        .map(|value| dwarf.attr_string(unit, value))
        .transpose()
        .map_err(DwarfError::from)
        .map(|value| value.map(|value| Arc::from(value.to_string_lossy().into_owned())))
}

/// Follows `DW_AT_abstract_origin`/`DW_AT_specification` references
/// transitively, rejecting cycles, so concrete inline-instance DIEs can
/// inherit name, type, and declaration metadata from their origins.
fn origin_chain<'data>(
    units: &[gimli::Unit<Reader<'data>>],
    unit_index: usize,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
) -> std::result::Result<Vec<(usize, gimli::DebuggingInformationEntry<Reader<'data>>)>, DwarfError>
{
    let mut chain = Vec::new();
    let mut visited = HashSet::new();
    let mut current = origin_reference(entry, unit_index, units)?;
    while let Some(key) = current {
        if !visited.insert(key) {
            return Err(DwarfError::ReferenceCycle);
        }
        let unit = units
            .get(key.unit)
            .ok_or(DwarfError::ReferenceOutsideUnits(key.offset))?;
        let origin = unit.entry(gimli::UnitOffset(key.offset))?;
        current = origin_reference(&origin, key.unit, units)?;
        chain.push((key.unit, origin));
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

fn copy_name_with_origins(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    units: &[gimli::Unit<Reader<'_>>],
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    chain: &[(usize, gimli::DebuggingInformationEntry<Reader<'_>>)],
) -> std::result::Result<Option<Arc<str>>, DwarfError> {
    if let Some(name) = copy_name(dwarf, unit, entry)? {
        return Ok(Some(name));
    }
    for (origin_unit, origin_entry) in chain {
        if let Some(name) = copy_name(dwarf, &units[*origin_unit], origin_entry)? {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

fn declaration_with_origins<'data>(
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
    Ok(Some(SourceLocation {
        file: source_file_id(path, source_files, source_file_ids),
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

fn copy_ranges<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    unit: &gimli::Unit<Reader<'data>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
) -> std::result::Result<Arc<[AddressRange<ImageAddress>]>, DwarfError> {
    let mut ranges = dwarf.die_ranges(unit, entry)?;
    let mut copied = Vec::new();
    while let Some(range) = ranges.next()? {
        if range.begin > range.end {
            return Err(DwarfError::InvalidRange);
        }
        if range.begin < range.end {
            copied.push(AddressRange {
                start: ImageAddress::new(range.begin),
                end: ImageAddress::new(range.end),
            });
        }
    }
    Ok(copied.into())
}

fn copy_optional_location(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    value: Option<gimli::AttributeValue<Reader<'_>>>,
) -> Metadata<LocationDescription> {
    let Some(value) = value else {
        return Metadata::Unavailable("no location was supplied".into());
    };
    match copy_location(dwarf, unit_index, unit, value) {
        Ok(location) => Metadata::Value(location),
        Err(error) => Metadata::Malformed(error.to_string().into()),
    }
}

fn copy_data_object_value(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
) -> Metadata<ValueDescription> {
    if let Some(location) = entry.attr_value(gimli::DW_AT_location) {
        return match copy_optional_location(dwarf, unit_index, unit, Some(location)) {
            Metadata::Value(location) => Metadata::Value(ValueDescription::Location(location)),
            Metadata::Unavailable(reason) => Metadata::Unavailable(reason),
            Metadata::Malformed(reason) => Metadata::Malformed(reason),
        };
    }
    if let Some(value) = entry.attr_value(gimli::DW_AT_const_value) {
        return match copy_constant(value) {
            Ok(value) => Metadata::Value(ValueDescription::Constant(value)),
            Err(error) => Metadata::Malformed(error),
        };
    }
    Metadata::Unavailable("no location was supplied".into())
}

fn copy_constant(
    value: gimli::AttributeValue<Reader<'_>>,
) -> std::result::Result<ConstantValue, Arc<str>> {
    Ok(match value {
        gimli::AttributeValue::Data1(value) => ConstantValue::Unsigned(u128::from(value)),
        gimli::AttributeValue::Data2(value) => ConstantValue::Unsigned(u128::from(value)),
        gimli::AttributeValue::Data4(value) => ConstantValue::Unsigned(u128::from(value)),
        gimli::AttributeValue::Data8(value) | gimli::AttributeValue::Udata(value) => {
            ConstantValue::Unsigned(u128::from(value))
        }
        gimli::AttributeValue::Data16(value) => ConstantValue::Unsigned(value),
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
    let encoding = unit.encoding();
    if let gimli::AttributeValue::Exprloc(expression) = value {
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

fn copy_expression(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    expression: gimli::Expression<Reader<'_>>,
    encoding: gimli::Encoding,
) -> std::result::Result<Expression, DwarfError> {
    let mut indexed_addresses = HashMap::new();
    let mut requires_frame_base = false;
    let mut operations = expression.operations(encoding);
    while let Some(operation) = operations.next()? {
        if matches!(operation, gimli::Operation::FrameOffset { .. }) {
            requires_frame_base = true;
        }
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
        requires_frame_base,
    })
}

fn load_evaluation_units(
    units: &[gimli::Unit<Reader<'_>>],
) -> std::result::Result<Vec<EvaluationUnit>, DwarfError> {
    units
        .iter()
        .map(|unit| {
            let mut base_types = HashMap::new();
            let mut entries = unit.entries();
            while let Some(entry) = entries.next_dfs()? {
                if entry.tag() != gimli::DW_TAG_base_type {
                    continue;
                }
                let Some(byte_size) = entry
                    .attr(gimli::DW_AT_byte_size)
                    .and_then(gimli::Attribute::udata_value)
                else {
                    continue;
                };
                let Some(raw_encoding) = entry
                    .attr(gimli::DW_AT_encoding)
                    .and_then(gimli::Attribute::udata_value)
                else {
                    continue;
                };
                let encoding = gimli::DwAte(u8::try_from(raw_encoding).unwrap_or(u8::MAX));
                if let Some(value_type) = dwarf_value_type(encoding, byte_size) {
                    base_types.insert(entry.offset().0, value_type);
                }
            }
            Ok(EvaluationUnit { base_types })
        })
        .collect()
}

const fn dwarf_value_type(encoding: gimli::DwAte, byte_size: u64) -> Option<gimli::ValueType> {
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

fn resolve_variable_type(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    units: &[gimli::Unit<Reader<'_>>],
    unit_index: usize,
    value: Option<gimli::AttributeValue<Reader<'_>>>,
) -> TypeResolution {
    let key = match die_reference(value, unit_index, units) {
        Ok(Some(key)) => key,
        Ok(None) => return TypeResolution::Malformed("variable has no type".into()),
        Err(error) => return TypeResolution::Malformed(error.to_string().into()),
    };
    resolve_type(dwarf, units, key, &mut HashSet::new())
}

fn resolve_type(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    units: &[gimli::Unit<Reader<'_>>],
    key: DieKey,
    visited: &mut HashSet<DieKey>,
) -> TypeResolution {
    if !visited.insert(key) {
        return TypeResolution::Malformed("type reference cycle".into());
    }
    let Some(unit) = units.get(key.unit) else {
        return TypeResolution::Malformed("type reference is outside loaded units".into());
    };
    let offset = gimli::UnitOffset(key.offset);
    let entry = match unit.entry(offset) {
        Ok(entry) => entry,
        Err(error) => return TypeResolution::Malformed(error.to_string().into()),
    };
    match entry.tag() {
        gimli::DW_TAG_base_type => {
            let name = match copy_name(dwarf, unit, &entry) {
                Ok(Some(name)) => name,
                Ok(None) => Arc::from("<unnamed base type>"),
                Err(error) => return TypeResolution::Malformed(error.to_string().into()),
            };
            let Some(byte_size) = entry
                .attr(gimli::DW_AT_byte_size)
                .and_then(gimli::Attribute::udata_value)
            else {
                return TypeResolution::Malformed("base type has no byte size".into());
            };
            if byte_size > MAX_SCALAR_BYTES {
                return TypeResolution::Unsupported(
                    format!("scalar type occupies {byte_size} bytes").into(),
                );
            }
            let Some(raw_encoding) = entry
                .attr(gimli::DW_AT_encoding)
                .and_then(gimli::Attribute::udata_value)
            else {
                return TypeResolution::Malformed("base type has no encoding".into());
            };
            let encoding = match gimli::DwAte(u8::try_from(raw_encoding).unwrap_or(u8::MAX)) {
                gimli::DW_ATE_boolean => BaseTypeEncoding::Boolean,
                gimli::DW_ATE_signed => BaseTypeEncoding::Signed,
                gimli::DW_ATE_signed_char => BaseTypeEncoding::SignedCharacter,
                gimli::DW_ATE_unsigned => BaseTypeEncoding::Unsigned,
                gimli::DW_ATE_unsigned_char => BaseTypeEncoding::UnsignedCharacter,
                gimli::DW_ATE_float => BaseTypeEncoding::Floating,
                other => {
                    return TypeResolution::Unsupported(
                        format!("base type encoding {other:?} is unsupported").into(),
                    );
                }
            };
            TypeResolution::Scalar(BaseType {
                name: Arc::clone(&name),
                base_name: name,
                encoding,
                byte_size,
            })
        }
        gimli::DW_TAG_typedef
        | gimli::DW_TAG_const_type
        | gimli::DW_TAG_volatile_type
        | gimli::DW_TAG_restrict_type => {
            let referenced =
                match die_reference(entry.attr_value(gimli::DW_AT_type), key.unit, units) {
                    Ok(Some(key)) => key,
                    Ok(None) => {
                        return TypeResolution::Malformed("type wrapper has no type".into());
                    }
                    Err(error) => return TypeResolution::Malformed(error.to_string().into()),
                };
            let mut resolved = resolve_type(dwarf, units, referenced, visited);
            if entry.tag() == gimli::DW_TAG_typedef
                && let TypeResolution::Scalar(type_info) = &mut resolved
                && let Ok(Some(name)) = copy_name(dwarf, unit, &entry)
            {
                type_info.name = name;
            }
            resolved
        }
        tag => TypeResolution::Unsupported(format!("type tag {tag:?} is unsupported").into()),
    }
}

impl VariableInfo for DwarfVariableInfo {
    fn inspect(
        &self,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        query: &VariableQuery,
        runtime: &mut dyn VariableRuntime,
    ) -> Result<Vec<Variable>> {
        let Some(function) = self.function_at(address) else {
            return match query {
                VariableQuery::All => Ok(Vec::new()),
                VariableQuery::Name(name) => Err(Error::VariableNotFound(name.clone())),
            };
        };
        // Source-level visibility is per logical frame: only data objects owned
        // by the selected inline instance (or the physical frame for `None`)
        // are in scope, even though siblings share the instruction address.
        let active = function
            .objects
            .iter()
            .map(|&index| &self.objects[index])
            .filter(|object| object.instance == selected)
            .filter(|object| object.ranges.iter().any(|range| range.contains(address)))
            .collect::<Vec<_>>();
        let selected_objects = match query {
            VariableQuery::All => active,
            VariableQuery::Name(name) => {
                let mut named = active
                    .into_iter()
                    .filter(|object| object.name.as_ref() == name)
                    .collect::<Vec<_>>();
                let Some(depth) = named.iter().map(|object| object.lexical_depth).max() else {
                    return Err(Error::VariableNotFound(name.clone()));
                };
                named.retain(|object| object.lexical_depth == depth);
                if named.len() != 1 {
                    return Err(Error::AmbiguousVariable(name.clone()));
                }
                named
            }
        };
        let mut selected = selected_objects;
        selected.sort_by_key(|object| {
            if object.kind == VariableKind::Parameter {
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
        });
        let mut frame_base = FrameBaseCache::Empty;
        Ok(selected
            .into_iter()
            .map(|object| self.inspect_data_object(object, address, runtime, &mut frame_base))
            .collect())
    }
}

impl DwarfVariableInfo {
    fn function_at(&self, address: ImageAddress) -> Option<&CatalogFunction> {
        self.address_index
            .range(..=address)
            .rev()
            .flat_map(|(_, functions)| functions.iter().copied())
            .find_map(|index| {
                let function = &self.functions[index];
                function
                    .ranges
                    .iter()
                    .any(|range| range.contains(address))
                    .then_some(function)
            })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "inspection preserves distinct malformed and unavailable metadata outcomes"
    )]
    fn inspect_data_object(
        &self,
        variable: &CatalogDataObject,
        address: ImageAddress,
        runtime: &mut dyn VariableRuntime,
        frame_base_cache: &mut FrameBaseCache,
    ) -> Variable {
        if let Some(description) = &variable.malformed {
            return malformed(variable, None, Arc::clone(description));
        }
        let type_info = match &variable.type_info {
            TypeResolution::Scalar(type_info) => type_info.clone(),
            TypeResolution::Unsupported(description) => {
                return unavailable(variable, None, Arc::clone(description).into());
            }
            TypeResolution::Malformed(description) => {
                return malformed(variable, None, Arc::clone(description));
            }
        };
        let description = match &variable.value {
            Metadata::Value(location) => location,
            Metadata::Unavailable(description) => {
                return unavailable(variable, Some(type_info), Arc::clone(description).into());
            }
            Metadata::Malformed(description) => {
                return malformed(variable, Some(type_info), Arc::clone(description));
            }
        };
        if let ValueDescription::Constant(constant) = description {
            let raw = match materialize_constant(constant, &type_info, self.target) {
                Ok(raw) => raw,
                Err(reason) => return unavailable(variable, Some(type_info), reason),
            };
            return available(
                variable,
                type_info,
                VariableValueSource::Constant,
                raw,
                self.target,
            );
        }
        let ValueDescription::Location(location) = description else {
            unreachable!("constant values returned above")
        };
        let expression = match location.expression(address) {
            Ok(Some(expression)) => expression,
            Err(reason) => return unavailable(variable, Some(type_info), reason),
            Ok(None) => {
                return unavailable(
                    variable,
                    Some(type_info),
                    "no location at the current instruction".into(),
                );
            }
        };
        let mut budget = EvaluationBudget::default();
        let frame_base = if expression.requires_frame_base {
            if matches!(frame_base_cache, FrameBaseCache::Empty) {
                *frame_base_cache = match &variable.frame_base {
                    Metadata::Value(frame_base) => match frame_base.expression(address) {
                        Ok(Some(expression)) => match evaluate_frame_base(
                            expression,
                            self.endian,
                            &self.evaluation_units,
                            runtime,
                            &mut budget,
                        ) {
                            Ok(value) => FrameBaseCache::Available(value),
                            Err(reason) => FrameBaseCache::Unavailable(reason),
                        },
                        Err(reason) => FrameBaseCache::Unavailable(reason),
                        Ok(None) => FrameBaseCache::Unavailable(
                            "no frame base at the current instruction".into(),
                        ),
                    },
                    Metadata::Unavailable(description) => {
                        FrameBaseCache::Unavailable(Arc::clone(description).into())
                    }
                    Metadata::Malformed(description) => {
                        FrameBaseCache::Malformed(Arc::clone(description))
                    }
                };
            }
            Some(match frame_base_cache {
                FrameBaseCache::Available(value) => *value,
                FrameBaseCache::Unavailable(reason) => {
                    return unavailable(variable, Some(type_info), reason.clone());
                }
                FrameBaseCache::Malformed(description) => {
                    return malformed(variable, Some(type_info), Arc::clone(description));
                }
                FrameBaseCache::Empty => unreachable!("frame base cache was populated"),
            })
        } else {
            None
        };
        let pieces = match evaluate(
            expression,
            self.endian,
            frame_base,
            &self.evaluation_units,
            runtime,
            &mut budget,
        ) {
            Ok(pieces) => pieces,
            Err(reason) => return unavailable(variable, Some(type_info), reason),
        };
        let (source, raw) = match materialize_pieces(
            &pieces,
            &type_info,
            self.endian,
            self.target,
            runtime,
            &mut budget,
        ) {
            Ok(value) => value,
            Err(reason) => return unavailable(variable, Some(type_info), reason),
        };
        available(variable, type_info, source, raw, self.target)
    }
}

fn available(
    variable: &CatalogDataObject,
    type_info: BaseType,
    source: VariableValueSource,
    raw: Arc<[u8]>,
    target: TargetDescription,
) -> Variable {
    let state = match decode_scalar(&type_info, &raw, target) {
        Ok(value) => VariableState::Available { source, raw, value },
        Err(reason) => VariableState::Unavailable(reason),
    };
    Variable {
        kind: variable.kind,
        name: Arc::clone(&variable.name),
        declaration: variable.declaration.clone(),
        type_info: Some(type_info),
        state,
    }
}

fn unavailable(
    variable: &CatalogDataObject,
    type_info: Option<BaseType>,
    reason: VariableUnavailableReason,
) -> Variable {
    Variable {
        kind: variable.kind,
        name: Arc::clone(&variable.name),
        declaration: variable.declaration.clone(),
        type_info,
        state: VariableState::Unavailable(reason),
    }
}

fn malformed(
    variable: &CatalogDataObject,
    type_info: Option<BaseType>,
    description: Arc<str>,
) -> Variable {
    Variable {
        kind: variable.kind,
        name: Arc::clone(&variable.name),
        declaration: variable.declaration.clone(),
        type_info,
        state: VariableState::Malformed(VariableMalformedReason { description }),
    }
}

fn evaluate_frame_base(
    expression: &Expression,
    endian: RunTimeEndian,
    units: &[EvaluationUnit],
    runtime: &mut dyn VariableRuntime,
    budget: &mut EvaluationBudget,
) -> std::result::Result<VirtualAddress, VariableUnavailableReason> {
    let pieces = evaluate(expression, endian, None, units, runtime, budget)?;
    let [piece] = pieces.as_slice() else {
        return Err("frame base is not one complete piece".into());
    };
    if piece.size_in_bits.is_some() || piece.bit_offset.is_some() {
        return Err("frame base is a partial piece".into());
    }
    match piece.location {
        Location::Address { address } => Ok(VirtualAddress::new(address)),
        Location::Register { register } => {
            register_u64(runtime, register.0, endian).map(VirtualAddress::new)
        }
        _ => Err("frame base did not evaluate to an address or register".into()),
    }
}

fn evaluate<'expression>(
    expression: &'expression Expression,
    endian: RunTimeEndian,
    frame_base: Option<VirtualAddress>,
    units: &[EvaluationUnit],
    runtime: &mut dyn VariableRuntime,
    budget: &mut EvaluationBudget,
) -> std::result::Result<Vec<gimli::Piece<Reader<'expression>>>, VariableUnavailableReason> {
    let reader = gimli::EndianSlice::new(&expression.bytes, endian);
    let mut evaluation = gimli::Expression(reader).evaluation(expression.encoding);
    // Bound evaluation so a malformed expression with a backward branch cannot
    // hang the controller thread.
    evaluation.set_max_iterations(MAX_EVALUATION_ITERATIONS);
    let mut result = evaluation.evaluate().map_err(evaluation_error)?;
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
                evaluation
                    .resume_with_register(value)
                    .map_err(evaluation_error)?
            }
            EvaluationResult::RequiresFrameBase => evaluation
                .resume_with_frame_base(
                    frame_base
                        .ok_or_else(|| Arc::<str>::from("frame base is unavailable"))?
                        .get(),
                )
                .map_err(evaluation_error)?,
            EvaluationResult::RequiresCallFrameCfa => evaluation
                .resume_with_call_frame_cfa(runtime.call_frame_cfa()?.get())
                .map_err(evaluation_error)?,
            EvaluationResult::RequiresRelocatedAddress(address) => evaluation
                .resume_with_relocated_address(runtime.relocate(ImageAddress::new(address))?.get())
                .map_err(evaluation_error)?,
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
                evaluation
                    .resume_with_indexed_address(address)
                    .map_err(evaluation_error)?
            }
            EvaluationResult::RequiresBaseType(offset) => evaluation
                .resume_with_base_type(evaluation_value_type(expression, units, offset.0)?)
                .map_err(evaluation_error)?,
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
                evaluation
                    .resume_with_memory(value)
                    .map_err(evaluation_error)?
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
            EvaluationResult::RequiresTls(_) => {
                return Err(crate::UnsupportedVariableFeature::Tls.into());
            }
            EvaluationResult::RequiresWasmLocal { .. }
            | EvaluationResult::RequiresWasmGlobal { .. }
            | EvaluationResult::RequiresWasmStack { .. } => {
                return Err(crate::UnsupportedVariableFeature::WasmLocation.into());
            }
        };
    }
}

fn evaluation_value_type(
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

fn evaluation_value(
    bytes: &[u8],
    value_type: gimli::ValueType,
    endian: RunTimeEndian,
) -> std::result::Result<Value, VariableUnavailableReason> {
    if value_type == gimli::ValueType::Generic {
        return bytes_to_u64(bytes, endian).map(Value::Generic);
    }
    let size = usize::try_from(value_type.bit_size(u64::MAX) / 8).expect("value size fits usize");
    if bytes.len() < size {
        return Err("register or memory value is shorter than its DWARF type".into());
    }
    let bytes = match endian {
        RunTimeEndian::Little => &bytes[..size],
        RunTimeEndian::Big => &bytes[bytes.len() - size..],
    };
    Value::parse(value_type, gimli::EndianSlice::new(bytes, endian)).map_err(evaluation_error)
}

fn materialize_pieces(
    pieces: &[gimli::Piece<Reader<'_>>],
    type_info: &BaseType,
    endian: RunTimeEndian,
    target: TargetDescription,
    runtime: &mut dyn VariableRuntime,
    budget: &mut EvaluationBudget,
) -> std::result::Result<(VariableValueSource, Arc<[u8]>), VariableUnavailableReason> {
    if pieces.len() > MAX_LOCATION_PIECES {
        return Err(VariableUnavailableReason::EvaluationLimit);
    }
    let [piece] = pieces else {
        return Err(crate::UnsupportedVariableFeature::CompositeLocation.into());
    };
    let expected_bits = type_info
        .byte_size
        .checked_mul(8)
        .ok_or_else(|| Arc::<str>::from("scalar bit size overflow"))?;
    if piece.size_in_bits.is_some_and(|size| size != expected_bits) || piece.bit_offset.is_some() {
        return Err(crate::UnsupportedVariableFeature::CompositeLocation.into());
    }
    let size = usize::try_from(type_info.byte_size).expect("scalar size fits usize");
    match piece.location {
        Location::Empty => Err(VariableUnavailableReason::OptimizedOut),
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
                .map_err(VariableUnavailableReason::Other)
        }
        Location::Register { register } => {
            let register = runtime.register(register.0)?;
            let raw = object_bytes(&register.bytes, size, endian)?;
            Ok((VariableValueSource::Register(register.descriptor), raw))
        }
        Location::Value { value } => Ok((
            VariableValueSource::Computed,
            dwarf_value_bytes(value, type_info, target)?,
        )),
        Location::Bytes { ref value } => {
            let bytes = value.to_slice().map_err(evaluation_error)?.into_owned();
            if bytes.len() != size {
                return Err("implicit value size does not match its scalar type".into());
            }
            Ok((VariableValueSource::Constant, bytes.into()))
        }
        Location::ImplicitPointer { .. } => {
            Err(crate::UnsupportedVariableFeature::ImplicitPointer.into())
        }
    }
}

fn object_bytes(
    bytes: &[u8],
    size: usize,
    endian: RunTimeEndian,
) -> std::result::Result<Arc<[u8]>, VariableUnavailableReason> {
    if bytes.len() < size {
        return Err("register value is shorter than the scalar type".into());
    }
    Ok(match endian {
        RunTimeEndian::Little => Arc::from(&bytes[..size]),
        RunTimeEndian::Big => Arc::from(&bytes[bytes.len() - size..]),
    })
}

fn dwarf_value_bytes(
    value: Value,
    type_info: &BaseType,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, VariableUnavailableReason> {
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
            return Err("computed floating-point size mismatch".into());
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

fn wrapping_integer_bytes(
    value: u128,
    size: usize,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, VariableUnavailableReason> {
    if size == 0 || size > 16 {
        return Err("unsupported computed integer size".into());
    }
    let value = value & low_bits_mask(size * 8);
    let bytes = match target.byte_order {
        ByteOrder::Little => value.to_le_bytes()[..size].to_vec(),
        ByteOrder::Big => value.to_be_bytes()[16 - size..].to_vec(),
    };
    Ok(bytes.into())
}

fn materialize_constant(
    value: &ConstantValue,
    type_info: &BaseType,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, VariableUnavailableReason> {
    let size = usize::try_from(type_info.byte_size).expect("scalar size fits usize");
    match value {
        ConstantValue::Unsigned(value) => integer_bytes(*value, size, target),
        ConstantValue::Signed(value) => signed_integer_bytes(*value, size, target),
        ConstantValue::Bytes(bytes) if bytes.len() == size => Ok(Arc::clone(bytes)),
        ConstantValue::Bytes(_) => Err("constant value size does not match its scalar type".into()),
    }
}

fn integer_bytes(
    value: u128,
    size: usize,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, VariableUnavailableReason> {
    if size == 0 || size > 16 || (size < 16 && value >= (1_u128 << (size * 8))) {
        return Err("constant value does not fit its scalar type".into());
    }
    let bytes = match target.byte_order {
        ByteOrder::Little => value.to_le_bytes()[..size].to_vec(),
        ByteOrder::Big => value.to_be_bytes()[16 - size..].to_vec(),
    };
    Ok(bytes.into())
}

fn signed_integer_bytes(
    value: i128,
    size: usize,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, VariableUnavailableReason> {
    if size == 0 || size > 16 {
        return Err("unsupported signed constant size".into());
    }
    let bits = size * 8;
    if bits < 128 {
        let minimum = -(1_i128 << (bits - 1));
        let maximum = (1_i128 << (bits - 1)) - 1;
        if !(minimum..=maximum).contains(&value) {
            return Err("signed constant value does not fit its scalar type".into());
        }
    }
    integer_bytes(value.cast_unsigned() & low_bits_mask(bits), size, target)
}

const fn low_bits_mask(bits: usize) -> u128 {
    if bits == 128 {
        u128::MAX
    } else {
        (1_u128 << bits) - 1
    }
}

fn register_u64(
    runtime: &mut dyn VariableRuntime,
    register: u16,
    endian: RunTimeEndian,
) -> std::result::Result<u64, VariableUnavailableReason> {
    let value = runtime.register(register)?;
    bytes_to_u64(&value.bytes, endian)
}

fn evaluation_error(error: gimli::Error) -> VariableUnavailableReason {
    VariableUnavailableReason::Other(format!("DWARF expression evaluation failed: {error}").into())
}

fn bytes_to_u64(
    bytes: &[u8],
    endian: RunTimeEndian,
) -> std::result::Result<u64, VariableUnavailableReason> {
    if bytes.len() > 8 {
        return Err("DWARF expression requested more than one word".into());
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

fn decode_scalar(
    type_info: &BaseType,
    bytes: &[u8],
    target: TargetDescription,
) -> std::result::Result<ScalarValue, VariableUnavailableReason> {
    let expected = usize::try_from(type_info.byte_size).expect("scalar byte size fits usize");
    if bytes.len() != expected {
        return Err("scalar storage size mismatch".into());
    }
    match type_info.encoding {
        BaseTypeEncoding::Boolean => match unsigned_value(bytes, target.byte_order)? {
            0 => Ok(ScalarValue::Boolean(false)),
            1 => Ok(ScalarValue::Boolean(true)),
            value => Err(format!("invalid boolean representation {value}").into()),
        },
        BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter => {
            Ok(ScalarValue::Signed(signed_value(bytes, target.byte_order)?))
        }
        BaseTypeEncoding::Unsigned | BaseTypeEncoding::UnsignedCharacter => Ok(
            ScalarValue::Unsigned(unsigned_value(bytes, target.byte_order)?),
        ),
        BaseTypeEncoding::Floating => decode_float(bytes, target).map(ScalarValue::Floating),
    }
}

fn unsigned_value(
    bytes: &[u8],
    byte_order: ByteOrder,
) -> std::result::Result<u128, VariableUnavailableReason> {
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

fn signed_value(
    bytes: &[u8],
    byte_order: ByteOrder,
) -> std::result::Result<i128, VariableUnavailableReason> {
    let unsigned = unsigned_value(bytes, byte_order)?;
    let bits = u32::try_from(bytes.len() * 8).expect("scalar bit count fits u32");
    if bits == 128 {
        return Ok(unsigned.cast_signed());
    }
    let shift = 128 - bits;
    Ok((unsigned << shift).cast_signed() >> shift)
}

fn decode_float(
    bytes: &[u8],
    target: TargetDescription,
) -> std::result::Result<FloatValue, VariableUnavailableReason> {
    match bytes.len() {
        4 => Ok(FloatValue::Binary32(
            u32::try_from(unsigned_value(bytes, target.byte_order)?).expect("four bytes fit u32"),
        )),
        8 => Ok(FloatValue::Binary64(
            u64::try_from(unsigned_value(bytes, target.byte_order)?).expect("eight bytes fit u64"),
        )),
        16 if target.architecture == Architecture::X86_64
            && target.byte_order == ByteOrder::Little =>
        {
            Ok(FloatValue::X87Extended {
                significand: u64::from_le_bytes(bytes[..8].try_into().expect("eight-byte slice")),
                sign_exponent: u16::from_le_bytes(bytes[8..10].try_into().expect("two-byte slice")),
            })
        }
        size => Err(format!("unsupported floating-point storage size {size}").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct Runtime {
        registers: BTreeMap<u16, u64>,
        cfa: std::result::Result<VirtualAddress, VariableUnavailableReason>,
        memory: Option<Arc<[u8]>>,
        memory_reads: u32,
    }

    impl VariableRuntime for Runtime {
        fn register(
            &mut self,
            register: u16,
        ) -> std::result::Result<crate::debug_info::VariableRegister, VariableUnavailableReason>
        {
            let value = self.registers.get(&register).copied().ok_or_else(|| {
                VariableUnavailableReason::RegisterUnavailable(register.to_string().into())
            })?;
            Ok(crate::debug_info::VariableRegister {
                descriptor: crate::RegisterDescriptor {
                    id: crate::RegisterId::new(u32::from(register)),
                    name: format!("r{register}").into(),
                    bits: 64,
                    role: None,
                },
                bytes: Arc::from(value.to_le_bytes()),
            })
        }

        fn call_frame_cfa(&self) -> std::result::Result<VirtualAddress, VariableUnavailableReason> {
            self.cfa.clone()
        }

        fn relocate(&self, address: ImageAddress) -> std::result::Result<VirtualAddress, Arc<str>> {
            Ok(VirtualAddress::new(address.get()))
        }

        fn read_memory(
            &mut self,
            _address: VirtualAddress,
            size: usize,
        ) -> std::result::Result<Arc<[u8]>, Arc<str>> {
            self.memory_reads += 1;
            self.memory
                .as_ref()
                .map(|memory| Arc::from(&memory[..size]))
                .ok_or_else(|| Arc::from("unexpected memory read"))
        }
    }

    fn expression(bytes: &[u8]) -> Expression {
        Expression {
            bytes: Arc::from(bytes),
            encoding: gimli::Encoding {
                format: gimli::Format::Dwarf32,
                version: 5,
                address_size: 8,
            },
            unit: 0,
            indexed_addresses: Arc::new(HashMap::new()),
            requires_frame_base: bytes.contains(&gimli::DW_OP_fbreg.0),
        }
    }

    fn scalar_type(encoding: BaseTypeEncoding, byte_size: u64) -> BaseType {
        BaseType {
            name: "test".into(),
            base_name: "test".into(),
            encoding,
            byte_size,
        }
    }

    fn target(byte_order: ByteOrder) -> TargetDescription {
        TargetDescription {
            architecture: Architecture::X86_64,
            byte_order,
            pointer_width: crate::PointerWidth::Bits64,
        }
    }

    fn units(
        base_types: impl IntoIterator<Item = (usize, gimli::ValueType)>,
    ) -> Vec<EvaluationUnit> {
        vec![EvaluationUnit {
            base_types: base_types.into_iter().collect(),
        }]
    }

    #[test]
    fn frame_base_register_and_fbreg_location_have_distinct_meanings() {
        let mut runtime = Runtime {
            registers: BTreeMap::from([(6, 0x2000)]),
            cfa: Ok(VirtualAddress::new(0x3000)),
            memory: None,
            memory_reads: 0,
        };
        let frame_base = evaluate_frame_base(
            &expression(&[gimli::DW_OP_reg6.0]),
            RunTimeEndian::Little,
            &units([]),
            &mut runtime,
            &mut EvaluationBudget::default(),
        )
        .expect("register-valued frame base");
        assert_eq!(frame_base, VirtualAddress::new(0x2000));

        let fbreg = expression(&[gimli::DW_OP_fbreg.0, 0x70]);
        let pieces = evaluate(
            &fbreg,
            RunTimeEndian::Little,
            Some(frame_base),
            &units([]),
            &mut runtime,
            &mut EvaluationBudget::default(),
        )
        .expect("frame-relative memory location");
        assert!(matches!(
            pieces.as_slice(),
            [gimli::Piece {
                location: Location::Address { address: 0x1ff0 },
                ..
            }]
        ));

        let direct_register = expression(&[gimli::DW_OP_reg6.0]);
        let pieces = evaluate(
            &direct_register,
            RunTimeEndian::Little,
            None,
            &units([]),
            &mut runtime,
            &mut EvaluationBudget::default(),
        )
        .expect("direct register location");
        assert!(matches!(
            pieces.as_slice(),
            [gimli::Piece {
                location: Location::Register {
                    register: gimli::Register(6)
                },
                ..
            }]
        ));
    }

    #[test]
    fn cfa_expression_limit_remains_a_typed_unavailable_reason() {
        let mut runtime = Runtime {
            registers: BTreeMap::new(),
            cfa: Err(VariableUnavailableReason::CfaExpression),
            memory: None,
            memory_reads: 0,
        };
        assert_eq!(
            evaluate_frame_base(
                &expression(&[gimli::DW_OP_call_frame_cfa.0]),
                RunTimeEndian::Little,
                &units([]),
                &mut runtime,
                &mut EvaluationBudget::default(),
            ),
            Err(VariableUnavailableReason::CfaExpression)
        );
    }

    #[test]
    fn malformed_backward_branch_expression_fails_instead_of_hanging() {
        let mut runtime = Runtime {
            registers: BTreeMap::new(),
            cfa: Ok(VirtualAddress::new(0x3000)),
            memory: None,
            memory_reads: 0,
        };
        // DW_OP_skip with a -3 offset branches back onto itself forever.
        let looping = expression(&[gimli::DW_OP_skip.0, 0xfd, 0xff]);
        let result = evaluate(
            &looping,
            RunTimeEndian::Little,
            None,
            &units([]),
            &mut runtime,
            &mut EvaluationBudget::default(),
        );
        assert!(result.is_err(), "infinite expression must be rejected");
    }

    #[test]
    fn typed_register_values_are_evaluated_with_the_referenced_base_type() {
        let mut runtime = Runtime {
            registers: BTreeMap::from([(6, 0x2000)]),
            cfa: Ok(VirtualAddress::new(0x3000)),
            memory: None,
            memory_reads: 0,
        };
        // DW_OP_regval_type register 6, base type DIE offset 0x10.
        let typed = expression(&[
            gimli::DW_OP_regval_type.0,
            6,
            0x10,
            gimli::DW_OP_stack_value.0,
        ]);
        let result = evaluate(
            &typed,
            RunTimeEndian::Little,
            None,
            &units([(0x10, gimli::ValueType::U64)]),
            &mut runtime,
            &mut EvaluationBudget::default(),
        )
        .expect("typed register expression");
        assert!(matches!(
            result.as_slice(),
            [gimli::Piece {
                location: Location::Value {
                    value: Value::U64(0x2000)
                },
                ..
            }]
        ));
    }

    #[test]
    fn implicit_and_computed_values_materialize_with_source_provenance() {
        let mut runtime = Runtime {
            registers: BTreeMap::new(),
            cfa: Ok(VirtualAddress::new(0x3000)),
            memory: None,
            memory_reads: 0,
        };
        let implicit = expression(&[gimli::DW_OP_implicit_value.0, 4, 0xd6, 0xff, 0xff, 0xff]);
        let pieces = evaluate(
            &implicit,
            RunTimeEndian::Little,
            None,
            &units([]),
            &mut runtime,
            &mut EvaluationBudget::default(),
        )
        .expect("implicit scalar expression");
        let materialized = materialize_pieces(
            &pieces,
            &scalar_type(BaseTypeEncoding::Signed, 4),
            RunTimeEndian::Little,
            target(ByteOrder::Little),
            &mut runtime,
            &mut EvaluationBudget::default(),
        )
        .expect("implicit scalar value");
        assert_eq!(materialized.0, VariableValueSource::Constant);
        assert_eq!(materialized.1.as_ref(), &[0xd6, 0xff, 0xff, 0xff]);

        let computed = dwarf_value_bytes(
            Value::Generic(u64::MAX - 1),
            &scalar_type(BaseTypeEncoding::Boolean, 1),
            target(ByteOrder::Little),
        )
        .expect("word-sized boolean expression");
        assert_eq!(computed.as_ref(), &[0]);
    }

    #[test]
    fn deferred_operations_and_missing_types_have_stable_typed_reasons() {
        let mut runtime = Runtime {
            registers: BTreeMap::from([(0, 1)]),
            cfa: Ok(VirtualAddress::new(0x3000)),
            memory: None,
            memory_reads: 0,
        };
        let entry = expression(&[
            gimli::DW_OP_entry_value.0,
            1,
            gimli::DW_OP_reg0.0,
            gimli::DW_OP_stack_value.0,
        ]);
        assert_eq!(
            evaluate(
                &entry,
                RunTimeEndian::Little,
                None,
                &units([]),
                &mut runtime,
                &mut EvaluationBudget::default(),
            ),
            Err(crate::UnsupportedVariableFeature::EntryValue.into())
        );

        let missing_type = expression(&[
            gimli::DW_OP_regval_type.0,
            0,
            0x10,
            gimli::DW_OP_stack_value.0,
        ]);
        assert_eq!(
            evaluate(
                &missing_type,
                RunTimeEndian::Little,
                None,
                &units([]),
                &mut runtime,
                &mut EvaluationBudget::default(),
            ),
            Err(crate::UnsupportedVariableFeature::TypedValue.into())
        );
    }

    #[test]
    fn expression_memory_reads_are_strictly_bounded() {
        let mut bytes = Vec::new();
        for address in 0..=MAX_EVALUATION_MEMORY_READS {
            bytes.push(gimli::DW_OP_addr.0);
            bytes.extend_from_slice(&u64::from(address).to_le_bytes());
            bytes.extend_from_slice(&[gimli::DW_OP_deref_size.0, 1, gimli::DW_OP_drop.0]);
        }
        bytes.extend_from_slice(&[gimli::DW_OP_lit0.0, gimli::DW_OP_stack_value.0]);
        let expression = expression(&bytes);
        let mut runtime = Runtime {
            registers: BTreeMap::new(),
            cfa: Ok(VirtualAddress::new(0x3000)),
            memory: Some(Arc::from([0_u8; 16])),
            memory_reads: 0,
        };
        assert_eq!(
            evaluate(
                &expression,
                RunTimeEndian::Little,
                None,
                &units([]),
                &mut runtime,
                &mut EvaluationBudget::default(),
            ),
            Err(VariableUnavailableReason::EvaluationLimit)
        );
        assert_eq!(runtime.memory_reads, MAX_EVALUATION_MEMORY_READS);
    }

    #[test]
    fn specific_location_entries_override_default_entries() {
        let range = |start: u64, end: u64| {
            Some(AddressRange {
                start: ImageAddress::new(start),
                end: ImageAddress::new(end),
            })
        };
        let description = LocationDescription {
            entries: vec![
                LocationEntry {
                    range: None,
                    expression: expression(&[gimli::DW_OP_reg0.0]),
                },
                LocationEntry {
                    range: range(0x100, 0x200),
                    expression: expression(&[gimli::DW_OP_reg1.0]),
                },
            ]
            .into(),
        };

        let specific = description
            .expression(ImageAddress::new(0x150))
            .expect("specific entry wins inside its range")
            .expect("an expression is active");
        assert_eq!(specific.bytes.as_ref(), &[gimli::DW_OP_reg1.0]);

        let fallback = description
            .expression(ImageAddress::new(0x300))
            .expect("default entry applies outside all ranges")
            .expect("an expression is active");
        assert_eq!(fallback.bytes.as_ref(), &[gimli::DW_OP_reg0.0]);

        let overlapping = LocationDescription {
            entries: vec![
                LocationEntry {
                    range: range(0x100, 0x200),
                    expression: expression(&[gimli::DW_OP_reg0.0]),
                },
                LocationEntry {
                    range: range(0x180, 0x280),
                    expression: expression(&[gimli::DW_OP_reg1.0]),
                },
            ]
            .into(),
        };
        assert!(overlapping.expression(ImageAddress::new(0x190)).is_err());
    }

    #[test]
    fn scalar_decoding_obeys_width_sign_and_target_byte_order() {
        for (bytes, byte_order, signed, unsigned) in [
            (&[0xfe][..], ByteOrder::Little, -2_i128, 254_u128),
            (&[0xfe, 0xff], ByteOrder::Little, -2, 65_534),
            (&[0xff, 0xfe], ByteOrder::Big, -2, 65_534),
            (
                &[0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
                ByteOrder::Little,
                -2,
                u128::from(u64::MAX - 1),
            ),
        ] {
            assert_eq!(
                decode_scalar(
                    &scalar_type(BaseTypeEncoding::Signed, bytes.len() as u64),
                    bytes,
                    target(byte_order),
                )
                .expect("signed scalar"),
                ScalarValue::Signed(signed)
            );
            assert_eq!(
                decode_scalar(
                    &scalar_type(BaseTypeEncoding::Unsigned, bytes.len() as u64),
                    bytes,
                    target(byte_order),
                )
                .expect("unsigned scalar"),
                ScalarValue::Unsigned(unsigned)
            );
        }
    }

    #[test]
    fn boolean_and_float_decoding_preserve_exact_representations() {
        let little = target(ByteOrder::Little);
        assert_eq!(
            decode_scalar(&scalar_type(BaseTypeEncoding::Boolean, 1), &[0], little,)
                .expect("false"),
            ScalarValue::Boolean(false)
        );
        assert!(decode_scalar(&scalar_type(BaseTypeEncoding::Boolean, 1), &[2], little,).is_err());
        assert_eq!(
            decode_scalar(
                &scalar_type(BaseTypeEncoding::Floating, 4),
                &1.25_f32.to_bits().to_le_bytes(),
                little,
            )
            .expect("binary32"),
            ScalarValue::Floating(FloatValue::Binary32(1.25_f32.to_bits()))
        );
        assert_eq!(
            decode_scalar(
                &scalar_type(BaseTypeEncoding::Floating, 8),
                &(-0.0_f64).to_bits().to_le_bytes(),
                little,
            )
            .expect("binary64"),
            ScalarValue::Floating(FloatValue::Binary64((-0.0_f64).to_bits()))
        );

        let mut extended = [0xa5_u8; 16];
        extended[..8].copy_from_slice(&0xc800_0000_0000_0000_u64.to_le_bytes());
        extended[8..10].copy_from_slice(&0x4000_u16.to_le_bytes());
        assert_eq!(
            decode_scalar(
                &scalar_type(BaseTypeEncoding::Floating, 16),
                &extended,
                little,
            )
            .expect("x87 extended"),
            ScalarValue::Floating(FloatValue::X87Extended {
                significand: 0xc800_0000_0000_0000,
                sign_exponent: 0x4000,
            })
        );
    }
}
