use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gimli::{
    BaseAddresses, CfaRule, ColumnType, DwarfSections, EhFrame, EndianSlice, RegisterRule,
    RunTimeEndian, SectionId, UnwindContext, UnwindSection,
};
use object::{Object, ObjectSection, ObjectSegment, ObjectSymbol};

use super::{DebugInfo, UnwindInfo};
use crate::model::{LineEntry, ModuleMetadata};
use crate::unwind::{MemoryReader, RegisterFile, UnwindStep};
use crate::{
    AddressRange, Architecture, BreakpointEntry, ByteOrder, CodeInstanceId, CodeInstanceInfo,
    CodeInstanceKind, ColumnNumber, EntryProvenance, Error, FunctionId, FunctionInfo, ImageAddress,
    LineNumber, LineSequenceId, ModuleImage, PointerWidth, Result, SourceFile, SourceFileId,
    SourceLocation, StatementFlags, StatementRow, SymbolId, SymbolInfo, TargetDescription,
    UnwindTermination, VirtualAddress,
};

#[derive(Debug, thiserror::Error)]
enum DwarfError {
    #[error("failed to read debug information: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse object file: {0}")]
    Object(#[from] object::Error),
    #[error("failed to parse DWARF: {0}")]
    Dwarf(#[from] gimli::Error),
    #[error("unsupported target architecture: {0:?}")]
    UnsupportedArchitecture(object::Architecture),
    #[error("unsupported supplementary DWARF reference")]
    UnsupportedSupplementaryReference,
    #[error("DWARF entry depth cannot be represented")]
    InvalidEntryDepth,
    #[error("DWARF code range is reversed")]
    InvalidRange,
    #[error("DWARF debug-info reference {0:#x} is outside every loaded unit")]
    ReferenceOutsideUnits(usize),
    #[error("unsupported DWARF reference form")]
    UnsupportedReferenceForm,
    #[error("DWARF reference targets an unsupported DIE at unit {unit}, offset {offset:#x}")]
    ReferencedFunctionMissing { unit: usize, offset: usize },
    #[error("DWARF reference cycle")]
    ReferenceCycle,
    #[error("concrete function has no source-level name")]
    MissingFunctionName,
}

type Reader<'data> = EndianSlice<'data, RunTimeEndian>;

mod variables;

struct DwarfUnwindInfo {
    eh_frame: Arc<[u8]>,
    endian: RunTimeEndian,
    address_size: u8,
    bases: BaseAddresses,
}

pub fn load(path: &Path) -> Result<DebugInfo> {
    load_debug_info(path).map_err(Error::debug_info)
}

fn load_debug_info(path: &Path) -> std::result::Result<DebugInfo, DwarfError> {
    let data = fs::read(path)?;
    let object = object::File::parse(data.as_slice())?;
    let target = target_description(&object)?;

    let sections = DwarfSections::load(
        |id: SectionId| -> std::result::Result<Cow<'_, [u8]>, DwarfError> {
            match object.section_by_name(id.name()) {
                Some(section) => Ok(section.uncompressed_data()?),
                None => Ok(Cow::Borrowed(&[])),
            }
        },
    )?;

    let endian = if object.is_little_endian() {
        RunTimeEndian::Little
    } else {
        RunTimeEndian::Big
    };

    let dwarf = sections.borrow(|section| EndianSlice::new(section, endian));
    let mut source_files = Vec::new();
    let mut source_file_ids = HashMap::new();
    let mut statements = Vec::new();
    let mut lines = Vec::new();
    let mut next_sequence = 0_u32;
    let mut unit_headers = dwarf.units();
    let mut units = Vec::new();

    while let Some(header) = unit_headers.next()? {
        units.push(dwarf.unit(header)?);
    }

    let (functions, code_instances) =
        load_function_metadata(&dwarf, &units, &mut source_files, &mut source_file_ids)?;

    for unit in &units {
        load_lines(
            &dwarf,
            unit,
            &mut source_files,
            &mut source_file_ids,
            &mut statements,
            &mut lines,
            &mut next_sequence,
        )?;
    }

    let variables = variables::load_variable_info(
        &dwarf,
        &units,
        target,
        &mut source_files,
        &mut source_file_ids,
    )?;
    let image = Arc::new(ModuleImage::new(
        path.to_owned(),
        target,
        image_address_range(&object)?,
        ModuleMetadata {
            functions,
            code_instances,
            symbols: load_symbols(&object),
            source_files,
            statements,
            lines,
        },
    ));
    let unwind = Arc::new(load_unwind_info(&object, target)?);

    Ok(DebugInfo {
        image,
        unwind,
        variables,
    })
}

fn image_address_range(
    object: &object::File<'_>,
) -> std::result::Result<AddressRange<ImageAddress>, DwarfError> {
    let mut start = u64::MAX;
    let mut end = 0;

    for segment in object.segments() {
        start = start.min(segment.address());
        end = end.max(
            segment
                .address()
                .checked_add(segment.size())
                .ok_or(gimli::Error::AddressOverflow)?,
        );
    }

    if start == u64::MAX {
        start = 0;
    }

    Ok(AddressRange {
        start: ImageAddress::new(start),
        end: ImageAddress::new(end),
    })
}

fn load_unwind_info(
    object: &object::File<'_>,
    target: TargetDescription,
) -> std::result::Result<DwarfUnwindInfo, DwarfError> {
    let section = object.section_by_name(".eh_frame");
    let eh_frame = section
        .as_ref()
        .map(ObjectSection::uncompressed_data)
        .transpose()?
        .unwrap_or(Cow::Borrowed(&[]))
        .into_owned()
        .into();
    let mut bases = BaseAddresses::default();

    if let Some(section) = section {
        bases = bases.set_eh_frame(section.address());
    }
    if let Some(section) = object.section_by_name(".text") {
        bases = bases.set_text(section.address());
    }
    if let Some(section) = object.section_by_name(".got") {
        bases = bases.set_got(section.address());
    }

    Ok(DwarfUnwindInfo {
        eh_frame,
        endian: match target.byte_order {
            ByteOrder::Little => RunTimeEndian::Little,
            ByteOrder::Big => RunTimeEndian::Big,
        },
        address_size: match target.pointer_width {
            PointerWidth::Bits32 => 4,
            PointerWidth::Bits64 => 8,
        },
        bases,
    })
}

impl UnwindInfo for DwarfUnwindInfo {
    fn cfa(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
    ) -> std::result::Result<VirtualAddress, UnwindTermination> {
        let mut section = EhFrame::new(&self.eh_frame, self.endian);
        section.set_address_size(self.address_size);
        let fde = section
            .fde_for_address(&self.bases, address.get(), EhFrame::cie_from_offset)
            .map_err(|error| cfi_error(error, address))?;
        let mut context = UnwindContext::new();
        let row = fde
            .unwind_info_for_address(&section, &self.bases, &mut context, address.get())
            .map_err(|error| cfi_error(error, address))?;
        cfa_from_rule(row.cfa(), registers)
    }

    fn unwind(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<UnwindStep, UnwindTermination> {
        let mut section = EhFrame::new(&self.eh_frame, self.endian);
        section.set_address_size(self.address_size);
        let fde = section
            .fde_for_address(&self.bases, address.get(), EhFrame::cie_from_offset)
            .map_err(|error| cfi_error(error, address))?;
        let return_register = fde.cie().return_address_register().0;
        let signal_frame = fde.cie().is_signal_trampoline();
        let mut context = UnwindContext::new();
        let row = fde
            .unwind_info_for_address(&section, &self.bases, &mut context, address.get())
            .map_err(|error| cfi_error(error, address))?;
        let cfa = cfa_from_rule(row.cfa(), registers)?;
        let mut caller = registers.clone();

        for &(register, ref rule) in row.registers() {
            apply_register_rule(&mut caller, registers, memory, cfa, register.0, rule)?;
        }
        caller.set(7, cfa.get());

        if caller.get(return_register).is_none() {
            return Err(UnwindTermination::Complete);
        }

        Ok(UnwindStep {
            registers: caller,
            cfa,
            signal_frame,
        })
    }
}

fn cfa_from_rule(
    rule: &CfaRule<usize>,
    registers: &RegisterFile,
) -> std::result::Result<VirtualAddress, UnwindTermination> {
    match rule {
        CfaRule::RegisterAndOffset { register, offset } => {
            let value = registers.get(register.0).ok_or_else(|| {
                UnwindTermination::RegisterUnavailable {
                    register: format!("DWARF register {}", register.0).into(),
                }
            })?;
            Ok(VirtualAddress::new(
                checked_add(value, *offset).ok_or_else(|| UnwindTermination::InvalidCaller {
                    description: "CFA arithmetic overflow".into(),
                })?,
            ))
        }
        CfaRule::Expression(_) => Err(UnwindTermination::UnsupportedUnwindInfo {
            feature: "CFA expression".into(),
        }),
    }
}

fn apply_register_rule(
    caller: &mut RegisterFile,
    current: &RegisterFile,
    memory: &mut dyn MemoryReader,
    cfa: VirtualAddress,
    register: u16,
    rule: &RegisterRule<usize>,
) -> std::result::Result<(), UnwindTermination> {
    let value = match rule {
        RegisterRule::Undefined => {
            caller.remove(register);
            return Ok(());
        }
        RegisterRule::SameValue => {
            current
                .get(register)
                .ok_or_else(|| UnwindTermination::RegisterUnavailable {
                    register: format!("DWARF register {register}").into(),
                })?
        }
        RegisterRule::Offset(offset) => {
            let address =
                VirtualAddress::new(checked_add(cfa.get(), *offset).ok_or_else(|| {
                    UnwindTermination::InvalidCaller {
                        description: "saved-register address overflow".into(),
                    }
                })?);
            memory
                .read_u64(address)
                .map_err(|()| UnwindTermination::MemoryReadFailed { address })?
        }
        RegisterRule::ValOffset(offset) => {
            checked_add(cfa.get(), *offset).ok_or_else(|| UnwindTermination::InvalidCaller {
                description: "register value overflow".into(),
            })?
        }
        RegisterRule::Register(source) => {
            current
                .get(source.0)
                .ok_or_else(|| UnwindTermination::RegisterUnavailable {
                    register: format!("DWARF register {}", source.0).into(),
                })?
        }
        RegisterRule::Constant(value) => *value,
        RegisterRule::Expression(_) | RegisterRule::ValExpression(_) => {
            return Err(UnwindTermination::UnsupportedUnwindInfo {
                feature: "register expression".into(),
            });
        }
        RegisterRule::Architectural => {
            return Err(UnwindTermination::UnsupportedUnwindInfo {
                feature: "architectural register rule".into(),
            });
        }
    };
    caller.set(register, value);
    Ok(())
}

const fn checked_add(value: u64, offset: i64) -> Option<u64> {
    if offset < 0 {
        value.checked_sub(offset.unsigned_abs())
    } else {
        value.checked_add(offset.unsigned_abs())
    }
}

fn cfi_error(error: gimli::Error, address: ImageAddress) -> UnwindTermination {
    if error == gimli::Error::NoUnwindInfoForAddress {
        UnwindTermination::NoUnwindInfo {
            address: VirtualAddress::new(address.get()),
        }
    } else {
        UnwindTermination::CorruptUnwindInfo {
            description: error.to_string().into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct DieKey {
    unit: usize,
    offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawFunctionKind {
    Subprogram,
    Inline,
}

struct RawFunction {
    key: DieKey,
    kind: RawFunctionKind,
    parent: Option<DieKey>,
    abstract_origin: Option<DieKey>,
    specification: Option<DieKey>,
    name: Option<Arc<str>>,
    linkage_name: Option<Arc<str>>,
    declaration: Option<SourceLocation>,
    call_site: Option<SourceLocation>,
    ranges: Vec<AddressRange<ImageAddress>>,
    entry: Option<ImageAddress>,
}

fn load_function_metadata(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    units: &[gimli::Unit<Reader<'_>>],
    source_files: &mut Vec<SourceFile>,
    source_file_ids: &mut HashMap<PathBuf, SourceFileId>,
) -> std::result::Result<(Vec<FunctionInfo>, Vec<CodeInstanceInfo>), DwarfError> {
    let raw = collect_function_dies(dwarf, units, source_files, source_file_ids)?;
    let by_key: HashMap<_, _> = raw
        .iter()
        .enumerate()
        .map(|(index, function)| (function.key, index))
        .collect();
    let mut functions = Vec::new();
    let mut function_ids = HashMap::new();

    for function in &raw {
        let definition = definition_key(function.key, &raw, &by_key)?;

        if function_ids.contains_key(&definition) {
            continue;
        }
        let name = inherited_value(definition, &raw, &by_key, |function| function.name.clone())?
            .ok_or(DwarfError::MissingFunctionName)?;
        let linkage_name = inherited_value(definition, &raw, &by_key, |function| {
            function.linkage_name.clone()
        })?;
        let declaration = inherited_value(definition, &raw, &by_key, |function| {
            function.declaration.clone()
        })?;
        let id = FunctionId::new(
            u32::try_from(functions.len()).map_err(|_| gimli::Error::UnsupportedOffset)?,
        );

        functions.push(FunctionInfo {
            id,
            name,
            linkage_name,
            declaration,
        });
        function_ids.insert(definition, id);
    }

    let mut code_instances = Vec::new();
    let mut instance_ids = HashMap::new();

    for function in &raw {
        if function.ranges.is_empty() {
            continue;
        }
        let definition = definition_key(function.key, &raw, &by_key)?;
        let id = CodeInstanceId::new(
            u32::try_from(code_instances.len()).map_err(|_| gimli::Error::UnsupportedOffset)?,
        );
        let parent = if function.kind == RawFunctionKind::Inline {
            containing_instance(function.parent, &raw, &by_key, &instance_ids)
        } else {
            None
        };
        let explicit_entry = function
            .entry
            .filter(|entry| function.ranges.iter().any(|range| range.contains(*entry)));
        let breakpoint_entry = explicit_entry
            .map(|address| BreakpointEntry {
                address,
                provenance: EntryProvenance::Explicit,
            })
            .or_else(|| {
                function.ranges.first().map(|range| BreakpointEntry {
                    address: range.start,
                    provenance: EntryProvenance::RangeStart,
                })
            });

        code_instances.push(CodeInstanceInfo {
            id,
            function: *function_ids
                .get(&definition)
                .expect("definition has a function ID"),
            parent,
            kind: match function.kind {
                RawFunctionKind::Subprogram => CodeInstanceKind::OutOfLine,
                RawFunctionKind::Inline => CodeInstanceKind::Inline {
                    call_site: function.call_site.clone(),
                },
            },
            ranges: function.ranges.clone().into(),
            breakpoint_entry,
        });
        instance_ids.insert(function.key, id);
    }

    Ok((functions, code_instances))
}

fn collect_function_dies(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    units: &[gimli::Unit<Reader<'_>>],
    source_files: &mut Vec<SourceFile>,
    source_file_ids: &mut HashMap<PathBuf, SourceFileId>,
) -> std::result::Result<Vec<RawFunction>, DwarfError> {
    let mut functions = Vec::new();

    for (unit_index, unit) in units.iter().enumerate() {
        let mut entries = unit.entries();
        let mut scopes = Vec::<Option<DieKey>>::new();

        while let Some(entry) = entries.next_dfs()? {
            let depth =
                usize::try_from(entry.depth()).map_err(|_| DwarfError::InvalidEntryDepth)?;
            scopes.truncate(depth);
            let parent = scopes.iter().rev().find_map(|key| *key);
            let kind = match entry.tag() {
                gimli::DW_TAG_subprogram => Some(RawFunctionKind::Subprogram),
                gimli::DW_TAG_inlined_subroutine => Some(RawFunctionKind::Inline),
                _ => None,
            };
            let key = DieKey {
                unit: unit_index,
                offset: entry.offset().0,
            };

            if let Some(kind) = kind {
                let mut ranges = dwarf.die_ranges(unit, entry)?;
                let mut concrete_ranges = Vec::new();

                while let Some(range) = ranges.next()? {
                    if range.begin > range.end {
                        return Err(DwarfError::InvalidRange);
                    }
                    if range.begin == range.end {
                        continue;
                    }
                    concrete_ranges.push(AddressRange {
                        start: ImageAddress::new(range.begin),
                        end: ImageAddress::new(range.end),
                    });
                }

                functions.push(RawFunction {
                    key,
                    kind,
                    parent,
                    abstract_origin: die_reference(
                        entry.attr_value(gimli::DW_AT_abstract_origin),
                        unit_index,
                        units,
                    )?,
                    specification: die_reference(
                        entry.attr_value(gimli::DW_AT_specification),
                        unit_index,
                        units,
                    )?,
                    name: attribute_string(dwarf, unit, entry.attr(gimli::DW_AT_name))?,
                    linkage_name: attribute_string(
                        dwarf,
                        unit,
                        entry.attr(gimli::DW_AT_linkage_name),
                    )?,
                    declaration: entry_source_location(
                        dwarf,
                        unit,
                        entry,
                        gimli::DW_AT_decl_file,
                        gimli::DW_AT_decl_line,
                        gimli::DW_AT_decl_column,
                        source_files,
                        source_file_ids,
                    )?,
                    call_site: entry_source_location(
                        dwarf,
                        unit,
                        entry,
                        gimli::DW_AT_call_file,
                        gimli::DW_AT_call_line,
                        gimli::DW_AT_call_column,
                        source_files,
                        source_file_ids,
                    )?,
                    ranges: concrete_ranges,
                    entry: entry
                        .attr(gimli::DW_AT_entry_pc)
                        .map(|attribute| dwarf.attr_address(unit, attribute.value()))
                        .transpose()?
                        .flatten()
                        .map(ImageAddress::new),
                });
                scopes.push(Some(key));
            } else {
                scopes.push(None);
            }
        }
    }

    Ok(functions)
}

fn attribute_string(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
    attribute: Option<&gimli::Attribute<Reader<'_>>>,
) -> std::result::Result<Option<Arc<str>>, DwarfError> {
    attribute
        .map(|attribute| dwarf.attr_string(unit, attribute.value()))
        .transpose()
        .map_err(DwarfError::from)
        .map(|value| value.map(|value| Arc::<str>::from(value.to_string_lossy().into_owned())))
}

fn die_reference(
    value: Option<gimli::AttributeValue<Reader<'_>>>,
    unit_index: usize,
    units: &[gimli::Unit<Reader<'_>>],
) -> std::result::Result<Option<DieKey>, DwarfError> {
    let Some(value) = value else {
        return Ok(None);
    };

    match value {
        gimli::AttributeValue::UnitRef(offset) => Ok(Some(DieKey {
            unit: unit_index,
            offset: offset.0,
        })),
        gimli::AttributeValue::DebugInfoRef(offset) => units
            .iter()
            .enumerate()
            .find_map(|(unit, candidate)| {
                offset
                    .to_unit_offset(&candidate.header)
                    .map(|offset| DieKey {
                        unit,
                        offset: offset.0,
                    })
            })
            .map(Some)
            .ok_or(DwarfError::ReferenceOutsideUnits(offset.0)),
        gimli::AttributeValue::DebugInfoRefSup(_) => {
            Err(DwarfError::UnsupportedSupplementaryReference)
        }
        _ => Err(DwarfError::UnsupportedReferenceForm),
    }
}

fn definition_key(
    start: DieKey,
    raw: &[RawFunction],
    by_key: &HashMap<DieKey, usize>,
) -> std::result::Result<DieKey, DwarfError> {
    let mut key = start;
    let mut visited = HashSet::new();

    loop {
        if !visited.insert(key) {
            return Err(DwarfError::ReferenceCycle);
        }
        let function = by_key.get(&key).and_then(|index| raw.get(*index)).ok_or(
            DwarfError::ReferencedFunctionMissing {
                unit: key.unit,
                offset: key.offset,
            },
        )?;
        let Some(next) = function.abstract_origin.or(function.specification) else {
            return Ok(key);
        };
        key = next;
    }
}

fn inherited_value<T>(
    start: DieKey,
    raw: &[RawFunction],
    by_key: &HashMap<DieKey, usize>,
    value: impl Fn(&RawFunction) -> Option<T>,
) -> std::result::Result<Option<T>, DwarfError> {
    let mut key = Some(start);
    let mut visited = HashSet::new();

    while let Some(current) = key {
        if !visited.insert(current) {
            return Err(DwarfError::ReferenceCycle);
        }
        let function = by_key
            .get(&current)
            .and_then(|index| raw.get(*index))
            .ok_or(DwarfError::ReferencedFunctionMissing {
                unit: current.unit,
                offset: current.offset,
            })?;

        if let Some(value) = value(function) {
            return Ok(Some(value));
        }
        key = function.abstract_origin.or(function.specification);
    }

    Ok(None)
}

fn containing_instance(
    mut key: Option<DieKey>,
    raw: &[RawFunction],
    by_key: &HashMap<DieKey, usize>,
    instances: &HashMap<DieKey, CodeInstanceId>,
) -> Option<CodeInstanceId> {
    while let Some(current) = key {
        if let Some(instance) = instances.get(&current) {
            return Some(*instance);
        }
        key = raw
            .get(*by_key.get(&current)?)
            .and_then(|function| function.parent);
    }

    None
}

#[allow(
    clippy::too_many_arguments,
    reason = "the three DWARF source attributes and shared interning state form one operation"
)]
fn entry_source_location(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    file_attribute: gimli::DwAt,
    line_attribute: gimli::DwAt,
    column_attribute: gimli::DwAt,
    source_files: &mut Vec<SourceFile>,
    source_file_ids: &mut HashMap<PathBuf, SourceFileId>,
) -> std::result::Result<Option<SourceLocation>, DwarfError> {
    let Some(file_index) = entry
        .attr(file_attribute)
        .and_then(gimli::Attribute::udata_value)
    else {
        return Ok(None);
    };
    let Some(line) = entry
        .attr(line_attribute)
        .and_then(gimli::Attribute::udata_value)
        .and_then(LineNumber::new)
    else {
        return Ok(None);
    };
    let Some(program) = unit.line_program.as_ref() else {
        return Ok(None);
    };
    let Some(file) = program.header().file(file_index) else {
        return Ok(None);
    };
    let path = source_path(dwarf, unit, program.header(), file)?;

    Ok(Some(SourceLocation {
        file: source_file_id(path, source_files, source_file_ids),
        line,
        column: entry
            .attr(column_attribute)
            .and_then(gimli::Attribute::udata_value)
            .and_then(ColumnNumber::new),
    }))
}

fn load_lines(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
    source_files: &mut Vec<SourceFile>,
    source_file_ids: &mut HashMap<PathBuf, SourceFileId>,
    statements: &mut Vec<StatementRow>,
    lines: &mut Vec<LineEntry>,
    next_sequence: &mut u32,
) -> std::result::Result<(), DwarfError> {
    let Some(program) = unit.line_program.clone() else {
        return Ok(());
    };
    let (program, sequences) = program.sequences()?;

    for sequence in sequences {
        let sequence_id = LineSequenceId::new(*next_sequence);
        *next_sequence = next_sequence
            .checked_add(1)
            .ok_or(gimli::Error::UnsupportedOffset)?;
        let mut rows = program.resume_from(&sequence);
        let mut previous: Option<(u64, SourceLocation)> = None;
        let mut ordinal = 0_u32;

        while let Some((header, row)) = rows.next_row()? {
            if row.end_sequence() {
                push_line_range(&mut previous, row.address(), lines);
                continue;
            }
            let Some(file) = row.file(header) else {
                continue;
            };
            let path = source_path(dwarf, unit, header, file)?;
            let file_id = source_file_id(path, source_files, source_file_ids);
            let Some(line) = row.line().and_then(|line| LineNumber::new(line.get())) else {
                continue;
            };
            let location = SourceLocation {
                file: file_id,
                line,
                column: match row.column() {
                    ColumnType::LeftEdge => None,
                    ColumnType::Column(column) => ColumnNumber::new(column.get()),
                },
            };

            statements.push(StatementRow {
                address: ImageAddress::new(row.address()),
                operation_index: row.op_index(),
                location: location.clone(),
                discriminator: row.discriminator(),
                flags: StatementFlags::empty()
                    .with_statement(row.is_stmt())
                    .with_basic_block(row.basic_block())
                    .with_prologue_end(row.prologue_end())
                    .with_epilogue_begin(row.epilogue_begin()),
                isa: row.isa(),
                sequence: sequence_id,
                ordinal,
            });
            ordinal = ordinal
                .checked_add(1)
                .ok_or(gimli::Error::UnsupportedOffset)?;

            push_line_range(&mut previous, row.address(), lines);
            previous = Some((row.address(), location));
        }
    }

    Ok(())
}

fn source_file_id(
    path: PathBuf,
    source_files: &mut Vec<SourceFile>,
    source_file_ids: &mut HashMap<PathBuf, SourceFileId>,
) -> SourceFileId {
    *source_file_ids.entry(path.clone()).or_insert_with(|| {
        let id = SourceFileId::new(
            u32::try_from(source_files.len()).expect("source file count fits in u32"),
        );
        source_files.push(SourceFile {
            id,
            path: Arc::new(path),
        });
        id
    })
}

fn source_path(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
    header: &gimli::LineProgramHeader<Reader<'_>>,
    file: &gimli::FileEntry<Reader<'_>>,
) -> std::result::Result<PathBuf, DwarfError> {
    let file_name = dwarf
        .attr_string(unit, file.path_name())?
        .to_string_lossy()
        .into_owned();
    let file_name = PathBuf::from(file_name);
    if file_name.is_absolute() {
        return Ok(file_name);
    }

    let directory = file
        .directory(header)
        .map(|directory| dwarf.attr_string(unit, directory))
        .transpose()?
        .map(|directory| PathBuf::from(directory.to_string_lossy().into_owned()));
    let compilation_directory = unit
        .comp_dir
        .as_ref()
        .map(|directory| PathBuf::from(directory.to_string_lossy().into_owned()));
    let mut path = PathBuf::new();

    if let Some(directory) = directory {
        if !directory.is_absolute()
            && let Some(compilation_directory) = compilation_directory
        {
            path.push(compilation_directory);
        }
        path.push(directory);
    } else if let Some(compilation_directory) = compilation_directory {
        path.push(compilation_directory);
    }
    path.push(file_name);

    Ok(path)
}

fn push_line_range(
    previous: &mut Option<(u64, SourceLocation)>,
    end: u64,
    lines: &mut Vec<LineEntry>,
) {
    if let Some((start, location)) = previous.take()
        && start < end
    {
        lines.push(LineEntry {
            range: AddressRange {
                start: ImageAddress::new(start),
                end: ImageAddress::new(end),
            },
            location,
        });
    }
}

fn load_symbols(object: &object::File<'_>) -> Vec<SymbolInfo> {
    let mut symbols_by_name: HashMap<String, Vec<u64>> = HashMap::new();

    for symbol in object.symbols().chain(object.dynamic_symbols()) {
        if symbol.address() == 0 {
            continue;
        }
        if let Ok(name) = symbol.name() {
            let addresses = symbols_by_name.entry(name.to_owned()).or_default();

            if !addresses.contains(&symbol.address()) {
                addresses.push(symbol.address());
            }
        }
    }

    symbols_by_name
        .into_iter()
        .flat_map(|(name, addresses)| {
            addresses
                .into_iter()
                .map(move |address| (name.clone(), address))
        })
        .enumerate()
        .map(|(id, (name, address))| SymbolInfo {
            id: SymbolId::new(u32::try_from(id).expect("symbol count fits in u32")),
            name: name.into(),
            address: ImageAddress::new(address),
        })
        .collect()
}

fn target_description(
    object: &object::File<'_>,
) -> std::result::Result<TargetDescription, DwarfError> {
    let architecture = match object.architecture() {
        object::Architecture::X86_64 => Architecture::X86_64,
        object::Architecture::Aarch64 => Architecture::Aarch64,
        other => return Err(DwarfError::UnsupportedArchitecture(other)),
    };

    Ok(TargetDescription {
        architecture,
        byte_order: if object.is_little_endian() {
            ByteOrder::Little
        } else {
            ByteOrder::Big
        },
        pointer_width: if object.is_64() {
            PointerWidth::Bits64
        } else {
            PointerWidth::Bits32
        },
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use gimli::Register;

    use super::*;

    struct TestMemory {
        values: BTreeMap<VirtualAddress, u64>,
    }

    impl MemoryReader for TestMemory {
        fn read_u64(&mut self, address: VirtualAddress) -> std::result::Result<u64, ()> {
            self.values.get(&address).copied().ok_or(())
        }
    }

    #[test]
    fn register_rules_distinguish_locations_values_and_frozen_registers() {
        let current = RegisterFile::new([(1, 100), (2, 200), (3, 300)]);
        let mut caller = current.clone();
        let cfa = VirtualAddress::new(0x1000);
        let mut memory = TestMemory {
            values: std::iter::once((VirtualAddress::new(0xff8), 0xfeed)).collect(),
        };

        apply_register_rule(
            &mut caller,
            &current,
            &mut memory,
            cfa,
            1,
            &RegisterRule::Constant(999),
        )
        .unwrap();
        apply_register_rule(
            &mut caller,
            &current,
            &mut memory,
            cfa,
            4,
            &RegisterRule::Register(Register(1)),
        )
        .unwrap();
        apply_register_rule(
            &mut caller,
            &current,
            &mut memory,
            cfa,
            5,
            &RegisterRule::Offset(-8),
        )
        .unwrap();
        apply_register_rule(
            &mut caller,
            &current,
            &mut memory,
            cfa,
            6,
            &RegisterRule::ValOffset(-8),
        )
        .unwrap();
        apply_register_rule(
            &mut caller,
            &current,
            &mut memory,
            cfa,
            3,
            &RegisterRule::Undefined,
        )
        .unwrap();

        assert_eq!(caller.get(1), Some(999));
        assert_eq!(caller.get(4), Some(100), "rule read mutated caller state");
        assert_eq!(caller.get(5), Some(0xfeed));
        assert_eq!(caller.get(6), Some(0xff8));
        assert_eq!(caller.get(3), None);
    }

    #[test]
    fn register_rule_failures_are_typed() {
        let current = RegisterFile::new([]);
        let mut caller = current.clone();
        let mut memory = TestMemory {
            values: BTreeMap::new(),
        };

        assert_eq!(
            apply_register_rule(
                &mut caller,
                &current,
                &mut memory,
                VirtualAddress::new(0),
                1,
                &RegisterRule::Offset(-1),
            ),
            Err(UnwindTermination::InvalidCaller {
                description: "saved-register address overflow".into()
            })
        );
        assert_eq!(
            apply_register_rule(
                &mut caller,
                &current,
                &mut memory,
                VirtualAddress::new(0x1000),
                1,
                &RegisterRule::Offset(0),
            ),
            Err(UnwindTermination::MemoryReadFailed {
                address: VirtualAddress::new(0x1000)
            })
        );
        assert_eq!(
            apply_register_rule(
                &mut caller,
                &current,
                &mut memory,
                VirtualAddress::new(0),
                1,
                &RegisterRule::Register(Register(9)),
            ),
            Err(UnwindTermination::RegisterUnavailable {
                register: "DWARF register 9".into()
            })
        );
    }
}
