use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use gimli::{EvaluationResult, Location, Reader as _, RunTimeEndian, Value};

use super::{DieKey, DwarfError, Reader, die_reference, entry_source_location};
use crate::debug_info::{VariableInfo, VariableRuntime};
use crate::{
    AddressRange, Architecture, BaseType, BaseTypeEncoding, ByteOrder, Error, FloatValue,
    ImageAddress, Result, ScalarValue, SourceFile, SourceFileId, SourceLocation, TargetDescription,
    Variable, VariableMalformedReason, VariableQuery, VariableState, VariableStorage,
    VariableUnavailableReason, VirtualAddress,
};

const MAX_SCALAR_BYTES: u64 = 16;

#[derive(Clone)]
struct Expression {
    bytes: Arc<[u8]>,
    encoding: gimli::Encoding,
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
        let mut matching = self
            .entries
            .iter()
            .filter(|entry| entry.range.is_none_or(|range| range.contains(address)));
        let expression = matching.next().map(|entry| &entry.expression);
        if matching.next().is_some() {
            return Err("multiple locations are active at the current instruction".into());
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
struct CatalogVariable {
    name: Arc<str>,
    declaration: Option<SourceLocation>,
    ranges: Arc<[AddressRange<ImageAddress>]>,
    lexical_depth: u32,
    order: u64,
    type_info: TypeResolution,
    location: Metadata<LocationDescription>,
    frame_base: Metadata<LocationDescription>,
    malformed: Option<Arc<str>>,
}

#[derive(Clone)]
struct CatalogParameter {
    name: Arc<str>,
    ranges: Arc<[AddressRange<ImageAddress>]>,
}

#[derive(Clone)]
struct Scope {
    ranges: Arc<[AddressRange<ImageAddress>]>,
    lexical_depth: u32,
    frame_base: Metadata<LocationDescription>,
    subprogram: bool,
    function: usize,
    malformed: Option<Arc<str>>,
}

struct CatalogFunction {
    ranges: Arc<[AddressRange<ImageAddress>]>,
    variables: Vec<usize>,
    parameters: Vec<usize>,
}

pub(super) struct DwarfVariableInfo {
    variables: Arc<[CatalogVariable]>,
    parameters: Arc<[CatalogParameter]>,
    functions: Arc<[CatalogFunction]>,
    address_index: BTreeMap<ImageAddress, Arc<[usize]>>,
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
    source_files: &mut Vec<SourceFile>,
    source_file_ids: &mut HashMap<PathBuf, SourceFileId>,
) -> std::result::Result<Arc<dyn VariableInfo>, DwarfError> {
    let mut variables = Vec::new();
    let mut parameters = Vec::new();
    let mut functions = Vec::new();
    let mut order = 0_u64;

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
                        variables: Vec::new(),
                        parameters: Vec::new(),
                    });
                    Some(Scope {
                        ranges,
                        lexical_depth: 0,
                        frame_base: copy_optional_location(
                            dwarf,
                            unit,
                            entry.attr_value(gimli::DW_AT_frame_base),
                        ),
                        subprogram: true,
                        function,
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
                        subprogram: false,
                        function: parent.function,
                        malformed: malformed.or_else(|| parent.malformed.clone()),
                    }
                }),
                gimli::DW_TAG_inlined_subroutine => None,
                tag if is_type_scope(tag) => None,
                _ => parent.clone(),
            };

            if entry.tag() == gimli::DW_TAG_variable {
                if let Some(scope) = parent.as_ref().filter(|scope| !scope.ranges.is_empty()) {
                    let (name, name_error) = match copy_name(dwarf, unit, entry) {
                        Ok(Some(name)) => (name, None),
                        Ok(None) => (
                            format!("<anonymous variable at {:#x}>", entry.offset().0).into(),
                            Some(Arc::from("variable has no name")),
                        ),
                        Err(error) => (
                            format!("<malformed variable at {:#x}>", entry.offset().0).into(),
                            Some(error.to_string().into()),
                        ),
                    };
                    order = order.checked_add(1).expect("variable DIE order overflow");
                    let declaration = entry_source_location(
                        dwarf,
                        unit,
                        entry,
                        gimli::DW_AT_decl_file,
                        gimli::DW_AT_decl_line,
                        gimli::DW_AT_decl_column,
                        source_files,
                        source_file_ids,
                    );
                    let (ranges, scope_error) = variable_scope_ranges(scope, entry);
                    functions[scope.function].variables.push(variables.len());
                    variables.push(CatalogVariable {
                        name,
                        declaration: declaration.as_ref().ok().cloned().flatten(),
                        ranges,
                        lexical_depth: scope.lexical_depth,
                        order,
                        type_info: resolve_variable_type(
                            dwarf,
                            units,
                            unit_index,
                            entry.attr_value(gimli::DW_AT_type),
                        ),
                        location: copy_optional_location(
                            dwarf,
                            unit,
                            entry.attr_value(gimli::DW_AT_location),
                        ),
                        frame_base: scope.frame_base.clone(),
                        malformed: declaration
                            .err()
                            .map(|error| error.to_string().into())
                            .or(scope_error)
                            .or_else(|| scope.malformed.clone())
                            .or(name_error),
                    });
                }
            } else if entry.tag() == gimli::DW_TAG_formal_parameter
                && let Some(scope) = parent.as_ref().filter(|scope| scope.subprogram)
                && let Ok(Some(name)) = copy_name(dwarf, unit, entry)
            {
                functions[scope.function].parameters.push(parameters.len());
                parameters.push(CatalogParameter {
                    name,
                    ranges: Arc::clone(&scope.ranges),
                });
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
        variables: variables.into(),
        parameters: parameters.into(),
        functions: functions.into(),
        address_index: address_index
            .into_iter()
            .map(|(address, functions)| (address, functions.into()))
            .collect(),
        target,
        endian: match target.byte_order {
            ByteOrder::Little => RunTimeEndian::Little,
            ByteOrder::Big => RunTimeEndian::Big,
        },
    }))
}

fn variable_scope_ranges(
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
    unit: &gimli::Unit<Reader<'_>>,
    value: Option<gimli::AttributeValue<Reader<'_>>>,
) -> Metadata<LocationDescription> {
    let Some(value) = value else {
        return Metadata::Unavailable("no location was supplied".into());
    };
    match copy_location(dwarf, unit, value) {
        Ok(location) => Metadata::Value(location),
        Err(error) => Metadata::Malformed(error.to_string().into()),
    }
}

fn copy_location(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
    value: gimli::AttributeValue<Reader<'_>>,
) -> std::result::Result<LocationDescription, DwarfError> {
    let encoding = unit.encoding();
    if let gimli::AttributeValue::Exprloc(expression) = value {
        return Ok(LocationDescription {
            entries: vec![LocationEntry {
                range: None,
                expression: copy_expression(expression, encoding)?,
            }]
            .into(),
        });
    }
    let mut locations = dwarf
        .attr_locations(unit, value)?
        .ok_or(DwarfError::UnsupportedReferenceForm)?;
    let mut entries = Vec::new();
    while let Some(location) = locations.next()? {
        if location.range.begin > location.range.end {
            return Err(DwarfError::InvalidRange);
        }
        if location.range.begin < location.range.end {
            entries.push(LocationEntry {
                range: Some(AddressRange {
                    start: ImageAddress::new(location.range.begin),
                    end: ImageAddress::new(location.range.end),
                }),
                expression: copy_expression(location.data, encoding)?,
            });
        }
    }
    Ok(LocationDescription {
        entries: entries.into(),
    })
}

fn copy_expression(
    expression: gimli::Expression<Reader<'_>>,
    encoding: gimli::Encoding,
) -> std::result::Result<Expression, DwarfError> {
    let bytes: Cow<'_, [u8]> = expression.0.to_slice()?;
    Ok(Expression {
        bytes: Arc::from(bytes.into_owned()),
        encoding,
    })
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
        query: &VariableQuery,
        runtime: &mut dyn VariableRuntime,
    ) -> Result<Vec<Variable>> {
        let Some(function) = self.function_at(address) else {
            return match query {
                VariableQuery::All => Ok(Vec::new()),
                VariableQuery::Name(name) => Err(Error::VariableNotFound(name.clone())),
            };
        };
        let active = function
            .variables
            .iter()
            .map(|&index| &self.variables[index])
            .filter(|variable| variable.ranges.iter().any(|range| range.contains(address)))
            .collect::<Vec<_>>();
        let selected = match query {
            VariableQuery::All => active,
            VariableQuery::Name(name) => {
                let mut named = active
                    .into_iter()
                    .filter(|variable| variable.name.as_ref() == name)
                    .collect::<Vec<_>>();
                let Some(depth) = named.iter().map(|variable| variable.lexical_depth).max() else {
                    if function.parameters.iter().any(|&index| {
                        let parameter = &self.parameters[index];
                        parameter.name.as_ref() == name
                            && parameter.ranges.iter().any(|range| range.contains(address))
                    }) {
                        return Err(Error::ParameterUnsupported(name.clone()));
                    }
                    return Err(Error::VariableNotFound(name.clone()));
                };
                named.retain(|variable| variable.lexical_depth == depth);
                if named.len() != 1 {
                    return Err(Error::AmbiguousVariable(name.clone()));
                }
                named
            }
        };
        let mut selected = selected;
        selected.sort_by_key(|variable| {
            variable.declaration.as_ref().map_or(
                (
                    1,
                    SourceFileId::new(u32::MAX),
                    u64::MAX,
                    u64::MAX,
                    variable.order,
                ),
                |location| {
                    (
                        0,
                        location.file,
                        location.line.get(),
                        location.column.map_or(0, crate::ColumnNumber::get),
                        variable.order,
                    )
                },
            )
        });
        Ok(selected
            .into_iter()
            .map(|variable| self.inspect_variable(variable, address, runtime))
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

    fn inspect_variable(
        &self,
        variable: &CatalogVariable,
        address: ImageAddress,
        runtime: &mut dyn VariableRuntime,
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
        let location = match &variable.location {
            Metadata::Value(location) => location,
            Metadata::Unavailable(description) => {
                return unavailable(variable, Some(type_info), Arc::clone(description).into());
            }
            Metadata::Malformed(description) => {
                return malformed(variable, Some(type_info), Arc::clone(description));
            }
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
        let frame_base = match &variable.frame_base {
            Metadata::Value(frame_base) => match frame_base.expression(address) {
                Ok(Some(expression)) => match evaluate_frame_base(expression, self.endian, runtime)
                {
                    Ok(value) => value,
                    Err(reason) => {
                        return unavailable(variable, Some(type_info), reason);
                    }
                },
                Err(reason) => return unavailable(variable, Some(type_info), reason),
                Ok(None) => {
                    return unavailable(
                        variable,
                        Some(type_info),
                        "no frame base at the current instruction".into(),
                    );
                }
            },
            Metadata::Unavailable(description) => {
                return unavailable(variable, Some(type_info), Arc::clone(description).into());
            }
            Metadata::Malformed(description) => {
                return malformed(variable, Some(type_info), Arc::clone(description));
            }
        };
        let storage = match evaluate_variable_location(expression, self.endian, frame_base, runtime)
        {
            Ok(address) => address,
            Err(description) => return unavailable(variable, Some(type_info), description),
        };
        let size = usize::try_from(type_info.byte_size).expect("scalar size fits usize");
        let raw = match runtime.read_memory(storage, size) {
            Ok(raw) => raw,
            Err(description) => return unavailable(variable, Some(type_info), description.into()),
        };
        let value = match decode_scalar(&type_info, &raw, self.target) {
            Ok(value) => value,
            Err(description) => return unavailable(variable, Some(type_info), description),
        };
        Variable {
            name: Arc::clone(&variable.name),
            declaration: variable.declaration.clone(),
            type_info: Some(type_info),
            state: VariableState::Available {
                storage: VariableStorage::Memory(storage),
                raw,
                value,
            },
        }
    }
}

fn unavailable(
    variable: &CatalogVariable,
    type_info: Option<BaseType>,
    reason: VariableUnavailableReason,
) -> Variable {
    Variable {
        name: Arc::clone(&variable.name),
        declaration: variable.declaration.clone(),
        type_info,
        state: VariableState::Unavailable(reason),
    }
}

fn malformed(
    variable: &CatalogVariable,
    type_info: Option<BaseType>,
    description: Arc<str>,
) -> Variable {
    Variable {
        name: Arc::clone(&variable.name),
        declaration: variable.declaration.clone(),
        type_info,
        state: VariableState::Malformed(VariableMalformedReason { description }),
    }
}

fn evaluate_frame_base(
    expression: &Expression,
    endian: RunTimeEndian,
    runtime: &mut dyn VariableRuntime,
) -> std::result::Result<VirtualAddress, VariableUnavailableReason> {
    let (pieces, _) = evaluate(expression, endian, None, runtime)?;
    let [piece] = pieces.as_slice() else {
        return Err("frame base is not one complete piece".into());
    };
    if piece.size_in_bits.is_some() || piece.bit_offset.is_some() {
        return Err("frame base is a partial piece".into());
    }
    match piece.location {
        Location::Address { address } => Ok(VirtualAddress::new(address)),
        Location::Register { register } => runtime
            .register(register.0)
            .map(VirtualAddress::new)
            .ok_or_else(|| format!("DWARF register {} is unavailable", register.0).into()),
        _ => Err("frame base did not evaluate to an address or register".into()),
    }
}

fn evaluate_variable_location(
    expression: &Expression,
    endian: RunTimeEndian,
    frame_base: VirtualAddress,
    runtime: &mut dyn VariableRuntime,
) -> std::result::Result<VirtualAddress, VariableUnavailableReason> {
    let (pieces, used_frame_base) = evaluate(expression, endian, Some(frame_base), runtime)?;
    if !used_frame_base {
        return Err("variable location is not frame-relative stack storage".into());
    }
    let [piece] = pieces.as_slice() else {
        return Err("variable location is not one complete piece".into());
    };
    if piece.size_in_bits.is_some() || piece.bit_offset.is_some() {
        return Err("variable location is a partial piece".into());
    }
    match piece.location {
        Location::Address { address } => Ok(VirtualAddress::new(address)),
        _ => Err("variable is not stored in memory".into()),
    }
}

fn evaluate<'expression>(
    expression: &'expression Expression,
    endian: RunTimeEndian,
    frame_base: Option<VirtualAddress>,
    runtime: &mut dyn VariableRuntime,
) -> std::result::Result<(Vec<gimli::Piece<Reader<'expression>>>, bool), VariableUnavailableReason>
{
    let reader = gimli::EndianSlice::new(&expression.bytes, endian);
    let mut evaluation = gimli::Expression(reader).evaluation(expression.encoding);
    let mut result = evaluation.evaluate().map_err(evaluation_error)?;
    let mut used_frame_base = false;
    loop {
        result = match result {
            EvaluationResult::Complete => return Ok((evaluation.result(), used_frame_base)),
            EvaluationResult::RequiresRegister { register, .. } => {
                let value = runtime.register(register.0).ok_or_else(|| {
                    Arc::<str>::from(format!("DWARF register {} is unavailable", register.0))
                })?;
                evaluation
                    .resume_with_register(Value::Generic(value))
                    .map_err(evaluation_error)?
            }
            EvaluationResult::RequiresFrameBase => {
                used_frame_base = true;
                evaluation
                    .resume_with_frame_base(
                        frame_base
                            .ok_or_else(|| Arc::<str>::from("frame base is unavailable"))?
                            .get(),
                    )
                    .map_err(evaluation_error)?
            }
            EvaluationResult::RequiresCallFrameCfa => evaluation
                .resume_with_call_frame_cfa(runtime.call_frame_cfa()?.get())
                .map_err(evaluation_error)?,
            EvaluationResult::RequiresRelocatedAddress(address) => evaluation
                .resume_with_relocated_address(runtime.relocate(ImageAddress::new(address))?.get())
                .map_err(evaluation_error)?,
            EvaluationResult::RequiresMemory {
                address,
                size,
                space: None,
                ..
            } => {
                let bytes = runtime.read_memory(VirtualAddress::new(address), usize::from(size))?;
                let value = bytes_to_u64(&bytes, endian)?;
                evaluation
                    .resume_with_memory(Value::Generic(value))
                    .map_err(evaluation_error)?
            }
            other => return Err(format!("unsupported DWARF evaluation request: {other:?}").into()),
        };
    }
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
    }

    impl VariableRuntime for Runtime {
        fn register(&self, register: u16) -> Option<u64> {
            self.registers.get(&register).copied()
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
            _size: usize,
        ) -> std::result::Result<Arc<[u8]>, Arc<str>> {
            Err("unexpected memory read".into())
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

    #[test]
    fn frame_base_register_and_fbreg_location_have_distinct_meanings() {
        let mut runtime = Runtime {
            registers: BTreeMap::from([(6, 0x2000)]),
            cfa: Ok(VirtualAddress::new(0x3000)),
        };
        let frame_base = evaluate_frame_base(
            &expression(&[gimli::DW_OP_reg6.0]),
            RunTimeEndian::Little,
            &mut runtime,
        )
        .expect("register-valued frame base");
        assert_eq!(frame_base, VirtualAddress::new(0x2000));

        let location = evaluate_variable_location(
            &expression(&[gimli::DW_OP_fbreg.0, 0x70]),
            RunTimeEndian::Little,
            frame_base,
            &mut runtime,
        )
        .expect("frame-relative memory location");
        assert_eq!(location, VirtualAddress::new(0x1ff0));

        assert!(
            evaluate_variable_location(
                &expression(&[gimli::DW_OP_reg6.0]),
                RunTimeEndian::Little,
                frame_base,
                &mut runtime,
            )
            .is_err()
        );
    }

    #[test]
    fn cfa_expression_limit_remains_a_typed_unavailable_reason() {
        let mut runtime = Runtime {
            registers: BTreeMap::new(),
            cfa: Err(VariableUnavailableReason::CfaExpression),
        };
        assert_eq!(
            evaluate_frame_base(
                &expression(&[gimli::DW_OP_call_frame_cfa.0]),
                RunTimeEndian::Little,
                &mut runtime,
            ),
            Err(VariableUnavailableReason::CfaExpression)
        );
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
