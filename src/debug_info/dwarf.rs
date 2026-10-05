use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gimli::{
    BaseAddresses, CfaRule, ColumnType, DebugFrame, DwarfSections, EhFrame, Encoding, EndianSlice,
    EvaluationResult, Location, RegisterRule, RunTimeEndian, SectionId, UnwindContext,
    UnwindExpression, UnwindSection, Value,
};
use object::{Object, ObjectSection, ObjectSegment};

use super::{DebugInfo, UnwindInfo};
use crate::model::{LineEntry, ModuleMetadata};
use crate::unwind::{MemoryReader, RegisterFile, UnwindStep};
use crate::{
    AddressRange, Architecture, BreakpointEntry, ByteOrder, CodeInstanceId, CodeInstanceInfo,
    CodeInstanceKind, ColumnNumber, EntryProvenance, Error, FunctionId, FunctionInfo, ImageAddress,
    LineNumber, LineSequenceId, ModuleImage, PointerWidth, Result, SourceFile, SourceFileId,
    SourceLocation, StatementFlags, StatementRow, TargetDescription, UnwindTermination,
    VirtualAddress,
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
    #[error("DWARF type signature {0:#018x} has no loaded definition")]
    TypeSignatureMissing(u64),
    #[error("DWARF type signature {0:#018x} has multiple definitions")]
    DuplicateTypeSignature(u64),
    #[error("DWARF reference targets an unsupported DIE at unit {unit}, offset {offset:#x}")]
    ReferencedFunctionMissing { unit: usize, offset: usize },
    #[error("DWARF reference cycle")]
    ReferenceCycle,
    #[error("malformed variable type metadata: {0}")]
    MalformedVariable(Arc<str>),
    #[error("DWARF data-object catalog exceeds {0} entries")]
    DataObjectLimit(usize),
    #[error("concrete function has no source-level name")]
    MissingFunctionName,
}

type Reader<'data> = EndianSlice<'data, RunTimeEndian>;
type TypeSignatures = HashMap<gimli::DebugTypeSignature, DieKey>;

struct UnitCatalog<'data> {
    units: Vec<gimli::Unit<Reader<'data>>>,
    type_signatures: TypeSignatures,
    code: CodeRanges,
}

/// The executable address ranges of an image.
///
/// When a linker discards a function, through `--gc-sections` or by merging
/// duplicate template instances, it points the function's debug information
/// at address 0 or a tombstone near `u64::MAX`. Those ranges lie outside
/// every executable section, which is how they are told apart from code.
struct CodeRanges(Vec<AddressRange<ImageAddress>>);

impl CodeRanges {
    fn contains(&self, range: AddressRange<ImageAddress>) -> bool {
        self.0
            .iter()
            .any(|code| code.start <= range.start && range.end <= code.end)
    }

    fn contains_address(&self, address: ImageAddress) -> bool {
        self.0.iter().any(|code| code.contains(address))
    }
}

/// Reads the code ranges a DIE covers, dropping empty ranges and the stubs
/// of discarded functions that lie outside the image's code.
fn die_code_ranges<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    unit: &gimli::Unit<Reader<'data>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
    code: &CodeRanges,
) -> std::result::Result<Vec<AddressRange<ImageAddress>>, DwarfError> {
    let mut ranges = Vec::new();
    if entry.attr_value(gimli::DW_AT_ranges).is_some() {
        let mut list = dwarf.die_ranges(unit, entry)?;
        while let Some(range) = list.next()? {
            ranges.push((range.begin, Some(range.end)));
        }
    } else if let (Some(low), Some(high)) = (
        entry.attr_value(gimli::DW_AT_low_pc),
        entry.attr_value(gimli::DW_AT_high_pc),
    ) && let Some(begin) = dwarf.attr_address(unit, low)?
    {
        // A constant high_pc is an offset from low_pc. gimli would add it
        // unchecked, which overflows for a tombstone low_pc.
        let end = dwarf
            .attr_address(unit, high)?
            .or_else(|| high.udata_value().and_then(|size| begin.checked_add(size)));
        ranges.push((begin, end));
    }
    Ok(ranges
        .into_iter()
        .filter_map(|(begin, end)| {
            let range = AddressRange {
                start: ImageAddress::new(begin),
                end: ImageAddress::new(end?),
            };
            (range.start < range.end && code.contains(range)).then_some(range)
        })
        .collect())
}

mod variables;

pub(in crate::debug_info) use variables::PathStep;

#[cfg(feature = "fuzzing")]
pub(super) fn fuzz_expression(data: &[u8]) {
    variables::fuzz_expression(data);
}

struct DwarfUnwindInfo {
    eh_frame: Arc<[u8]>,
    debug_frame: Arc<[u8]>,
    endian: RunTimeEndian,
    address_size: u8,
    bases: BaseAddresses,
    /// Code the Go toolchain compiled, sorted by start address. Go's calling
    /// convention lets a callee overwrite registers the System V ABI
    /// preserves.
    go_code: Vec<AddressRange<ImageAddress>>,
}

pub fn load(path: &Path, image_id: crate::ModuleImageId) -> Result<DebugInfo> {
    let data: Arc<[u8]> = fs::read(path)?.into();
    load_debug_info(path, &data, image_id).map_err(Error::debug_info)
}

pub fn load_bytes(path: &Path, data: &[u8], image_id: crate::ModuleImageId) -> Result<DebugInfo> {
    load_debug_info(path, data, image_id).map_err(Error::debug_info)
}

fn load_debug_info(
    path: &Path,
    data: &[u8],
    image_id: crate::ModuleImageId,
) -> std::result::Result<DebugInfo, DwarfError> {
    let object = object::File::parse(data)?;
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
    let mut type_unit_headers = dwarf.type_units();
    while let Some(header) = type_unit_headers.next()? {
        units.push(dwarf.unit(header)?);
    }
    let catalog = UnitCatalog {
        type_signatures: type_signature_index(&units)?,
        units,
        code: CodeRanges(super::elf::executable_ranges(&object)),
    };

    let mut function_metadata =
        load_function_metadata(&dwarf, &catalog, &mut source_files, &mut source_file_ids)?;

    for unit in catalog.units.iter().filter(|unit| !is_type_unit(unit)) {
        load_lines(
            &dwarf,
            unit,
            &catalog.code,
            &mut source_files,
            &mut source_file_ids,
            &mut statements,
            &mut lines,
            &mut next_sequence,
        )?;
    }

    refine_proved_prologue_entries(
        &object,
        target,
        &statements,
        &mut function_metadata.code_instances,
    );

    let variables = variables::load_variable_info(
        &dwarf,
        &catalog,
        target,
        image_id,
        &function_metadata.instance_ids,
        &mut source_files,
        &mut source_file_ids,
    )?;
    let go_code = go_code_ranges(&dwarf, &catalog)?;
    let unwind = Arc::new(load_unwind_info(&object, target, go_code)?);
    let symbols = super::elf::load_symbols(&object, &unwind.function_ranges());
    let image = Arc::new(
        ModuleImage::new(
            path.to_owned(),
            target,
            image_address_range(&object)?,
            ModuleMetadata {
                functions: function_metadata.functions,
                code_instances: function_metadata.code_instances,
                symbols: symbols.symbols,
                symbol_sources: symbols.sources,
                globals: variables.globals,
                types: variables.types,
                source_files,
                statements,
                lines,
                sections: super::elf::load_sections(&object),
            },
        )
        .with_id(image_id),
    );

    Ok(DebugInfo {
        image,
        unwind,
        variables: variables.info,
    })
}

/// Returns the code ranges of every unit written in Go, merged and sorted
/// by start address. A Go unit's ranges also cover the assembly functions
/// of its package, which follow the same calling convention.
fn go_code_ranges<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    catalog: &UnitCatalog<'data>,
) -> std::result::Result<Vec<AddressRange<ImageAddress>>, DwarfError> {
    let mut ranges = Vec::new();
    for unit in catalog.units.iter().filter(|unit| !is_type_unit(unit)) {
        let mut entries = unit.entries();
        let Some(root) = entries.next_dfs()? else {
            continue;
        };
        if matches!(
            root.attr_value(gimli::DW_AT_language),
            Some(gimli::AttributeValue::Language(gimli::DW_LANG_Go))
        ) {
            ranges.extend(die_code_ranges(dwarf, unit, root, &catalog.code)?);
        }
    }
    // Merging lets a lookup check only the last range starting at or before
    // an address.
    ranges.sort_unstable_by_key(|range| range.start);
    let mut merged: Vec<AddressRange<ImageAddress>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    Ok(merged)
}

fn type_signature_index(
    units: &[gimli::Unit<Reader<'_>>],
) -> std::result::Result<TypeSignatures, DwarfError> {
    let mut signatures = HashMap::new();
    for (unit_index, unit) in units.iter().enumerate() {
        let (gimli::UnitType::Type {
            type_signature,
            type_offset,
        }
        | gimli::UnitType::SplitType {
            type_signature,
            type_offset,
        }) = unit.header.type_()
        else {
            continue;
        };
        if signatures
            .insert(
                type_signature,
                DieKey {
                    unit: unit_index,
                    offset: type_offset.0,
                },
            )
            .is_some()
        {
            return Err(DwarfError::DuplicateTypeSignature(type_signature.0));
        }
    }
    Ok(signatures)
}

fn is_type_unit(unit: &gimli::Unit<Reader<'_>>) -> bool {
    matches!(
        unit.header.type_(),
        gimli::UnitType::Type { .. } | gimli::UnitType::SplitType { .. }
    )
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
    go_code: Vec<AddressRange<ImageAddress>>,
) -> std::result::Result<DwarfUnwindInfo, DwarfError> {
    let section = object.section_by_name(".eh_frame");
    let eh_frame = section
        .as_ref()
        .map(ObjectSection::uncompressed_data)
        .transpose()?
        .unwrap_or(Cow::Borrowed(&[]))
        .into_owned()
        .into();
    let debug_frame = object
        .section_by_name(".debug_frame")
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
        debug_frame,
        endian: match target.byte_order {
            ByteOrder::Little => RunTimeEndian::Little,
            ByteOrder::Big => RunTimeEndian::Big,
        },
        address_size: match target.pointer_width {
            PointerWidth::Bits32 => 4,
            PointerWidth::Bits64 => 8,
        },
        bases,
        go_code,
    })
}

impl DwarfUnwindInfo {
    /// Returns the code range of every function the call-frame information
    /// describes. Enumeration stops at the first malformed entry, so the
    /// result is evidence of function boundaries rather than a complete map.
    fn function_ranges(&self) -> Vec<AddressRange<ImageAddress>> {
        let mut ranges = Vec::new();
        let mut eh_frame = EhFrame::new(&self.eh_frame, self.endian);
        eh_frame.set_address_size(self.address_size);
        collect_function_ranges(&eh_frame, &self.bases, &mut ranges);
        let mut debug_frame = DebugFrame::new(&self.debug_frame, self.endian);
        debug_frame.set_address_size(self.address_size);
        collect_function_ranges(&debug_frame, &self.bases, &mut ranges);
        ranges
    }

    /// Returns the registers the function at `address` may overwrite
    /// without saving them, by the calling convention it follows.
    fn call_clobbered_registers(&self, address: ImageAddress) -> &'static [u16] {
        let after = self.go_code.partition_point(|range| range.start <= address);
        if after > 0 && self.go_code[after - 1].contains(address) {
            &X86_64_GO_CALL_CLOBBERED_REGISTERS
        } else {
            &X86_64_SYSV_CALL_CLOBBERED_REGISTERS
        }
    }
}

fn collect_function_ranges<'data, S>(
    section: &S,
    bases: &BaseAddresses,
    ranges: &mut Vec<AddressRange<ImageAddress>>,
) where
    S: UnwindSection<Reader<'data>>,
{
    let mut entries = section.entries(bases);
    while let Ok(Some(entry)) = entries.next() {
        let gimli::CieOrFde::Fde(partial) = entry else {
            continue;
        };
        let Ok(fde) = partial.parse(S::cie_from_offset) else {
            continue;
        };
        let start = fde.initial_address();
        if let Some(end) = start.checked_add(fde.len())
            && start < end
        {
            ranges.push(AddressRange {
                start: ImageAddress::new(start),
                end: ImageAddress::new(end),
            });
        }
    }
}

impl UnwindInfo for DwarfUnwindInfo {
    fn cfa(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<VirtualAddress, UnwindTermination> {
        let mut eh_frame = EhFrame::new(&self.eh_frame, self.endian);
        eh_frame.set_address_size(self.address_size);
        let result = cfa_from_section(&eh_frame, &self.bases, address, registers, memory);
        if !matches!(result, Err(UnwindTermination::NoUnwindInfo { .. })) {
            return result;
        }

        let mut debug_frame = DebugFrame::new(&self.debug_frame, self.endian);
        debug_frame.set_address_size(self.address_size);
        cfa_from_section(&debug_frame, &self.bases, address, registers, memory)
    }

    fn unwind(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<UnwindStep, UnwindTermination> {
        let mut eh_frame = EhFrame::new(&self.eh_frame, self.endian);
        eh_frame.set_address_size(self.address_size);
        let clobbered = self.call_clobbered_registers(address);
        let result = unwind_from_section(
            &eh_frame,
            &self.bases,
            address,
            registers,
            clobbered,
            memory,
        );
        if !matches!(result, Err(UnwindTermination::NoUnwindInfo { .. })) {
            return result;
        }

        let mut debug_frame = DebugFrame::new(&self.debug_frame, self.endian);
        debug_frame.set_address_size(self.address_size);
        unwind_from_section(
            &debug_frame,
            &self.bases,
            address,
            registers,
            clobbered,
            memory,
        )
    }
}

fn cfa_from_section<'data, S>(
    section: &S,
    bases: &BaseAddresses,
    address: ImageAddress,
    registers: &RegisterFile,
    memory: &mut dyn MemoryReader,
) -> std::result::Result<VirtualAddress, UnwindTermination>
where
    S: UnwindSection<Reader<'data>>,
{
    let fde = section
        .fde_for_address(bases, address.get(), S::cie_from_offset)
        .map_err(|error| cfi_error(error, address))?;
    let encoding = fde.cie().encoding();
    let mut context = UnwindContext::new();
    let row = fde
        .unwind_info_for_address(section, bases, &mut context, address.get())
        .map_err(|error| cfi_error(error, address))?;
    cfa_from_rule(row.cfa(), registers, section, encoding, memory)
}

fn unwind_from_section<'data, S>(
    section: &S,
    bases: &BaseAddresses,
    address: ImageAddress,
    registers: &RegisterFile,
    clobbered: &[u16],
    memory: &mut dyn MemoryReader,
) -> std::result::Result<UnwindStep, UnwindTermination>
where
    S: UnwindSection<Reader<'data>>,
{
    let fde = section
        .fde_for_address(bases, address.get(), S::cie_from_offset)
        .map_err(|error| cfi_error(error, address))?;
    let return_register = fde.cie().return_address_register().0;
    let signal_frame = fde.cie().is_signal_trampoline();
    let encoding = fde.cie().encoding();
    let mut context = UnwindContext::new();
    let row = fde
        .unwind_info_for_address(section, bases, &mut context, address.get())
        .map_err(|error| cfi_error(error, address))?;
    let cfa = cfa_from_rule(row.cfa(), registers, section, encoding, memory)?;
    let mut caller = registers.clone();
    // A callee may overwrite every register its calling convention does not
    // preserve across calls, so the caller's value survives only where the
    // row says where it was saved. Keeping the callee's value would present
    // it as the caller's.
    for &register in clobbered {
        if !row
            .registers()
            .any(|(described, _)| described.0 == register)
        {
            caller.remove(register);
        }
    }

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

/// The DWARF numbers of the registers the x86-64 System V ABI lets a callee
/// overwrite: rax, rdx, rcx, rsi, rdi, r8-r11, rflags, the SSE registers,
/// and the x87 stack.
const X86_64_SYSV_CALL_CLOBBERED_REGISTERS: [u16; 34] = [
    0, 1, 2, 4, 5, 8, 9, 10, 11, 49, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
    32, 33, 34, 35, 36, 37, 38, 39, 40,
];

/// The DWARF numbers of the registers Go code may overwrite: every System V
/// call-clobbered register, and rbx, rbp, and r12-r15 too. Go's register ABI
/// preserves none of them across calls, assembly functions may overwrite the
/// goroutine pointer in r14, and Go's call-frame information does not
/// describe the frame pointer a prologue saves.
const X86_64_GO_CALL_CLOBBERED_REGISTERS: [u16; 40] = [
    0, 1, 2, 3, 4, 5, 6, 8, 9, 10, 11, 12, 13, 14, 15, 49, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40,
];

fn cfa_from_rule<'data, S>(
    rule: &CfaRule<usize>,
    registers: &RegisterFile,
    section: &S,
    encoding: Encoding,
    memory: &mut dyn MemoryReader,
) -> std::result::Result<VirtualAddress, UnwindTermination>
where
    S: UnwindSection<Reader<'data>>,
{
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
        CfaRule::Expression(expression) => {
            evaluate_unwind_expression(expression, section, encoding, registers, memory)
        }
    }
}

// Bounds unwind-expression evaluation so a malformed expression with a
// backward branch cannot hang the controller thread.
const MAX_UNWIND_EXPRESSION_ITERATIONS: u32 = 10_000;

fn evaluate_unwind_expression<'data, S>(
    expression: &UnwindExpression<usize>,
    section: &S,
    encoding: Encoding,
    registers: &RegisterFile,
    memory: &mut dyn MemoryReader,
) -> std::result::Result<VirtualAddress, UnwindTermination>
where
    S: UnwindSection<Reader<'data>>,
{
    let unsupported = |feature: &str| UnwindTermination::UnsupportedUnwindInfo {
        feature: format!("CFA expression: {feature}").into(),
    };
    let corrupt = |error: gimli::Error| UnwindTermination::CorruptUnwindInfo {
        description: format!("CFA expression: {error}").into(),
    };
    let expression = expression.get(section).map_err(corrupt)?;
    let mut evaluation = expression.evaluation(encoding);
    evaluation.set_max_iterations(MAX_UNWIND_EXPRESSION_ITERATIONS);
    let mut result = evaluation.evaluate().map_err(corrupt)?;
    loop {
        result = match result {
            EvaluationResult::Complete => break,
            EvaluationResult::RequiresRegister { register, .. } => {
                let value = registers.get(register.0).ok_or_else(|| {
                    UnwindTermination::RegisterUnavailable {
                        register: format!("DWARF register {}", register.0).into(),
                    }
                })?;
                evaluation
                    .resume_with_register(Value::Generic(value))
                    .map_err(corrupt)?
            }
            EvaluationResult::RequiresMemory { space: Some(_), .. } => {
                return Err(unsupported("non-default memory address space"));
            }
            EvaluationResult::RequiresMemory { address, size, .. } => {
                if size == 0 || u32::from(size) > 8 {
                    return Err(unsupported("unsupported memory operand size"));
                }
                let address = VirtualAddress::new(address);
                let word = memory
                    .read_u64(address)
                    .ok_or(UnwindTermination::MemoryReadFailed { address })?;
                let bits = u32::from(size) * 8;
                let value = if bits == 64 {
                    word
                } else {
                    word & ((1 << bits) - 1)
                };
                evaluation
                    .resume_with_memory(Value::Generic(value))
                    .map_err(corrupt)?
            }
            EvaluationResult::RequiresFrameBase => return Err(unsupported("frame base")),
            EvaluationResult::RequiresTls(_) => return Err(unsupported("TLS")),
            EvaluationResult::RequiresCallFrameCfa => {
                return Err(unsupported("recursive CFA"));
            }
            _ => return Err(unsupported("unsupported expression operation")),
        };
    }

    let pieces = evaluation.result();
    let [piece] = pieces.as_slice() else {
        return Err(unsupported("compound location"));
    };
    match piece.location {
        Location::Address { address } => Ok(VirtualAddress::new(address)),
        _ => Err(unsupported("non-address result")),
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
                .ok_or(UnwindTermination::MemoryReadFailed { address })?
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

struct FunctionMetadata {
    functions: Vec<FunctionInfo>,
    code_instances: Vec<CodeInstanceInfo>,
    /// Maps each concrete function DIE to its code instance so the variable
    /// catalog can attribute scopes to logical frames.
    instance_ids: HashMap<DieKey, CodeInstanceId>,
}

fn load_function_metadata(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    catalog: &UnitCatalog<'_>,
    source_files: &mut Vec<SourceFile>,
    source_file_ids: &mut HashMap<PathBuf, SourceFileId>,
) -> std::result::Result<FunctionMetadata, DwarfError> {
    let raw = collect_function_dies(dwarf, catalog, source_files, source_file_ids)?;
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

    Ok(FunctionMetadata {
        functions,
        code_instances,
        instance_ids,
    })
}

fn collect_function_dies(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    catalog: &UnitCatalog<'_>,
    source_files: &mut Vec<SourceFile>,
    source_file_ids: &mut HashMap<PathBuf, SourceFileId>,
) -> std::result::Result<Vec<RawFunction>, DwarfError> {
    let units = catalog.units.as_slice();
    let mut functions = Vec::new();

    for (unit_index, unit) in units.iter().enumerate() {
        if is_type_unit(unit) {
            continue;
        }
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
                let concrete_ranges = die_code_ranges(dwarf, unit, entry, &catalog.code)?;
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

fn die_reference_with_signatures(
    value: Option<gimli::AttributeValue<Reader<'_>>>,
    unit_index: usize,
    units: &[gimli::Unit<Reader<'_>>],
    signatures: &TypeSignatures,
) -> std::result::Result<Option<DieKey>, DwarfError> {
    match value {
        Some(gimli::AttributeValue::DebugTypesRef(signature)) => signatures
            .get(&signature)
            .copied()
            .map(Some)
            .ok_or(DwarfError::TypeSignatureMissing(signature.0)),
        value => die_reference(value, unit_index, units),
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

#[expect(
    clippy::too_many_arguments,
    reason = "line loading appends to every per-image table the loader builds"
)]
fn load_lines(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
    code: &CodeRanges,
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
    // Rows name files by index into the program header; resolving a path
    // allocates, so each index is resolved once.
    let mut file_ids = HashMap::new();

    for sequence in sequences {
        // A discarded function's sequence starts outside the image's code.
        if !code.contains_address(ImageAddress::new(sequence.start)) {
            continue;
        }
        let sequence_id = LineSequenceId::new(*next_sequence);
        *next_sequence = next_sequence
            .checked_add(1)
            .ok_or(gimli::Error::UnsupportedOffset)?;
        let mut rows = program.resume_from(&sequence);
        let mut previous: Option<(u64, SourceLocation, bool)> = None;
        let mut ordinal = 0_u32;

        while let Some((header, row)) = rows.next_row()? {
            if row.end_sequence() {
                push_line_range(&mut previous, row.address(), lines);
                continue;
            }
            let flags = StatementFlags::empty()
                .with_statement(row.is_stmt())
                .with_basic_block(row.basic_block())
                .with_prologue_end(row.prologue_end())
                .with_epilogue_begin(row.epilogue_begin());
            let row_ordinal = ordinal;
            ordinal = ordinal
                .checked_add(1)
                .ok_or(gimli::Error::UnsupportedOffset)?;

            // Rows without a resolvable file or with line 0 mark compiler-
            // generated code with no source attribution. They still terminate
            // the previous entry's range; extending it would misattribute the
            // gap to a neighboring source line. A prologue or epilogue marker
            // remains actionable even when that source attribution is absent.
            let location = match (
                row.file(header),
                row.line().and_then(|line| LineNumber::new(line.get())),
            ) {
                (Some(file), Some(line)) => {
                    let file = if let Some(&id) = file_ids.get(&row.file_index()) {
                        id
                    } else {
                        let path = source_path(dwarf, unit, header, file)?;
                        let id = source_file_id(path, source_files, source_file_ids);
                        file_ids.insert(row.file_index(), id);
                        id
                    };
                    Some(SourceLocation {
                        file,
                        line,
                        column: match row.column() {
                            ColumnType::LeftEdge => None,
                            ColumnType::Column(column) => ColumnNumber::new(column.get()),
                        },
                    })
                }
                _ => None,
            };

            if location.is_some() || flags.prologue_end() || flags.epilogue_begin() {
                statements.push(StatementRow {
                    address: ImageAddress::new(row.address()),
                    operation_index: row.op_index(),
                    location: location.clone(),
                    discriminator: row.discriminator(),
                    flags,
                    isa: row.isa(),
                    sequence: sequence_id,
                    ordinal: row_ordinal,
                });
            }

            let Some(location) = location else {
                push_line_range(&mut previous, row.address(), lines);
                continue;
            };

            // Rows at one address collapse into a single entry, a statement
            // boundary if any collapsed row recommends it. Its location is
            // the last statement row's, as gdb presents it: a later row that
            // is no statement, such as the line an inlined call came from,
            // does not describe where execution stands.
            if let Some((start, _, true)) = &previous
                && *start == row.address()
                && !row.is_stmt()
            {
                continue;
            }
            let statement = row.is_stmt()
                || previous
                    .as_ref()
                    .is_some_and(|(start, _, statement)| *start == row.address() && *statement);
            push_line_range(&mut previous, row.address(), lines);
            previous = Some((row.address(), location, statement));
        }
    }

    Ok(())
}

fn source_file_id(
    path: PathBuf,
    source_files: &mut Vec<SourceFile>,
    source_file_ids: &mut HashMap<PathBuf, SourceFileId>,
) -> SourceFileId {
    if let Some(&id) = source_file_ids.get(&path) {
        return id;
    }
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

fn type_unit_source_file_id(
    path: PathBuf,
    source_files: &mut Vec<SourceFile>,
    source_file_ids: &mut HashMap<PathBuf, SourceFileId>,
) -> SourceFileId {
    if path.is_relative() {
        #[expect(
            clippy::disallowed_methods,
            reason = "only a unique match is used, which no iteration order changes"
        )]
        let mut suffix_matches = source_file_ids
            .iter()
            .filter(|(candidate, _)| candidate.is_absolute() && candidate.ends_with(&path))
            .map(|(_, id)| *id);
        if let Some(id) = suffix_matches.next()
            && suffix_matches.next().is_none()
        {
            return id;
        }
    }
    source_file_id(path, source_files, source_file_ids)
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
    previous: &mut Option<(u64, SourceLocation, bool)>,
    end: u64,
    lines: &mut Vec<LineEntry>,
) {
    if let Some((start, location, statement)) = previous.take()
        && start < end
    {
        lines.push(LineEntry {
            range: AddressRange {
                start: ImageAddress::new(start),
                end: ImageAddress::new(end),
            },
            location,
            statement,
        });
    }
}

/// Moves an out-of-line function's breakpoint entry past a prologue that
/// x86-64 instruction analysis proves only sets up the frame, when the line
/// table marks no `prologue_end`. A heuristic: anything unproven keeps the
/// raw entry, and no failure here fails the module.
fn refine_proved_prologue_entries(
    object: &object::File<'_>,
    target: TargetDescription,
    statements: &[StatementRow],
    instances: &mut [CodeInstanceInfo],
) {
    if target.architecture != Architecture::X86_64 {
        return;
    }
    let rows = StatementIndex::new(statements);

    for instance in instances {
        if !matches!(instance.kind, CodeInstanceKind::OutOfLine)
            || instance.ranges.iter().any(|range| {
                rows.within(*range)
                    .iter()
                    .any(|row| row.flags.prologue_end())
            })
        {
            continue;
        }
        let Some(raw_entry) = instance.breakpoint_entry.map(|entry| entry.address) else {
            continue;
        };
        let Some(entry_range) = instance
            .ranges
            .iter()
            .find(|range| range.contains(raw_entry))
        else {
            continue;
        };
        let Some(candidate) = first_distinct_source_statement(&rows, *entry_range, raw_entry)
        else {
            continue;
        };
        let Some(bytes) = code_bytes(object, raw_entry.get(), candidate.get()) else {
            continue;
        };

        #[cfg(target_arch = "x86_64")]
        if super::x86_64::prove_prologue_prefix(bytes, raw_entry.get()).is_ok() {
            instance.breakpoint_entry = Some(BreakpointEntry {
                address: candidate,
                provenance: EntryProvenance::AnalyzedPrologue,
            });
        }
    }
}

/// Statement rows sorted by address, keeping line-program order among rows
/// at one address.
struct StatementIndex<'a>(Vec<&'a StatementRow>);

impl<'a> StatementIndex<'a> {
    fn new(statements: &'a [StatementRow]) -> Self {
        let mut rows = statements.iter().collect::<Vec<_>>();
        rows.sort_by_key(|row| row.address);
        Self(rows)
    }

    fn within(&self, range: AddressRange<ImageAddress>) -> &[&'a StatementRow] {
        let start = self.0.partition_point(|row| row.address < range.start);
        let end = self.0.partition_point(|row| row.address < range.end);
        &self.0[start..end.max(start)]
    }
}

fn first_distinct_source_statement(
    rows: &StatementIndex<'_>,
    range: AddressRange<ImageAddress>,
    raw_entry: ImageAddress,
) -> Option<ImageAddress> {
    // Overlapping line programs (COMDAT folding, duplicated metadata) can
    // attribute the same image address from unrelated sequences. Prologue
    // reasoning is only sound within the single sequence that describes the
    // entry, so an ambiguous entry attribution keeps the raw entry.
    let entry_end = ImageAddress::new(raw_entry.get().checked_add(1)?);
    let mut entry_rows = rows
        .within(AddressRange {
            start: raw_entry,
            end: entry_end,
        })
        .iter()
        .filter(|row| row.location.is_some());
    let entry_row = entry_rows.next_back()?;
    if entry_rows.any(|row| row.sequence != entry_row.sequence) {
        return None;
    }
    // Line programs collapse equal-address rows by taking the final source
    // attribution. Mirror that rule here, and do not mistake a later row for
    // the same signature line for proof that argument homing has completed.
    let entry_location = entry_row.location.as_ref()?;
    rows.within(AddressRange {
        start: entry_end,
        end: range.end,
    })
    .iter()
    .filter(|row| row.sequence == entry_row.sequence && row.flags.is_statement())
    .find(|row| {
        row.location.as_ref().is_some_and(|location| {
            location.file != entry_location.file || location.line != entry_location.line
        })
    })
    .map(|row| row.address)
}

/// Returns the bytes of `[start, end)` from an executable section.
fn code_bytes<'data>(
    object: &'data object::File<'data>,
    start: u64,
    end: u64,
) -> Option<&'data [u8]> {
    let length = usize::try_from(end.checked_sub(start)?).ok()?;
    let section = object.sections().find(|section| {
        section.kind() == object::SectionKind::Text
            && section.address() <= start
            && section
                .address()
                .checked_add(section.size())
                .is_some_and(|section_end| end <= section_end)
    })?;
    let offset = usize::try_from(start - section.address()).ok()?;
    section
        .data()
        .ok()?
        .get(offset..offset.checked_add(length)?)
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
    use std::collections::{BTreeMap, HashMap};

    use gimli::write::{
        Address, Dwarf as WriteDwarf, EndianVec, LineProgram, LineString, Sections, Unit,
    };
    use gimli::{Encoding, Format, LineEncoding, LittleEndian, Register};

    use super::*;

    #[test]
    fn type_signature_references_resolve_only_indexed_primary_dies() {
        let signature = gimli::DebugTypeSignature(0x1234_5678_9abc_def0);
        let key = DieKey {
            unit: 3,
            offset: 0x40,
        };
        let signatures = HashMap::from([(signature, key)]);
        let units = Vec::<gimli::Unit<Reader<'_>>>::new();

        assert_eq!(
            die_reference_with_signatures(
                Some(gimli::AttributeValue::DebugTypesRef(signature)),
                0,
                &units,
                &signatures,
            )
            .expect("indexed signature"),
            Some(key)
        );
        assert!(matches!(
            die_reference_with_signatures(
                Some(gimli::AttributeValue::DebugTypesRef(
                    gimli::DebugTypeSignature(7)
                )),
                0,
                &units,
                &signatures,
            ),
            Err(DwarfError::TypeSignatureMissing(7))
        ));
    }

    #[test]
    fn relative_type_unit_source_paths_coalesce_only_with_a_unique_absolute_suffix() {
        let mut files = Vec::new();
        let mut ids = HashMap::new();
        let absolute = type_unit_source_file_id(
            PathBuf::from("/work/project/src/types.cpp"),
            &mut files,
            &mut ids,
        );
        assert_eq!(
            type_unit_source_file_id(PathBuf::from("src/types.cpp"), &mut files, &mut ids),
            absolute
        );
        assert_eq!(files.len(), 1);

        type_unit_source_file_id(
            PathBuf::from("/other/project/src/types.cpp"),
            &mut files,
            &mut ids,
        );
        let ambiguous_relative =
            type_unit_source_file_id(PathBuf::from("src/types.cpp"), &mut files, &mut ids);
        assert_ne!(ambiguous_relative, absolute);
        assert_eq!(files.len(), 3);
    }

    struct TestMemory {
        values: BTreeMap<VirtualAddress, u64>,
    }

    #[test]
    fn line_loader_retains_unattributed_control_boundaries_and_equal_address_order() {
        let encoding = Encoding {
            format: Format::Dwarf32,
            version: 4,
            address_size: 8,
        };
        let mut program = LineProgram::new(
            encoding,
            LineEncoding::default(),
            LineString::String(b"/test".to_vec()),
            None,
            LineString::String(b"boundary.c".to_vec()),
            None,
        );
        let file = program.add_file(
            LineString::String(b"boundary.c".to_vec()),
            program.default_directory(),
            None,
        );
        program.begin_sequence(Some(Address::Constant(0x100)));
        program.row().file = file;
        program.row().line = 0;
        program.row().is_statement = false;
        program.row().prologue_end = true;
        program.generate_row();
        program.row().file = file;
        program.row().line = 10;
        program.row().is_statement = true;
        program.row().epilogue_begin = true;
        program.generate_row();
        program.end_sequence(4);

        let mut written = WriteDwarf::new();
        written.units.add(Unit::new(encoding, program));
        let mut sections = Sections::new(EndianVec::new(LittleEndian));
        written.write(&mut sections).expect("write test DWARF");
        let dwarf = gimli::Dwarf::load(|id| {
            let bytes = sections.get(id).map(EndianVec::slice).unwrap_or_default();
            Ok::<_, gimli::Error>(EndianSlice::new(bytes, RunTimeEndian::Little))
        })
        .expect("read test DWARF");
        let mut headers = dwarf.units();
        let header = headers.next().unwrap().expect("one test unit");
        let unit = dwarf.unit(header).expect("read test unit");
        let mut source_files = Vec::new();
        let mut source_file_ids = HashMap::new();
        let mut statements = Vec::new();
        let mut lines = Vec::new();
        let mut next_sequence = 0;

        let code = |start, end| {
            CodeRanges(vec![AddressRange {
                start: ImageAddress::new(start),
                end: ImageAddress::new(end),
            }])
        };
        // A sequence outside the image's code belongs to a discarded function.
        load_lines(
            &dwarf,
            &unit,
            &code(0x200, 0x300),
            &mut source_files,
            &mut source_file_ids,
            &mut statements,
            &mut lines,
            &mut next_sequence,
        )
        .expect("load test line program");
        assert!(statements.is_empty() && lines.is_empty());

        load_lines(
            &dwarf,
            &unit,
            &code(0x100, 0x200),
            &mut source_files,
            &mut source_file_ids,
            &mut statements,
            &mut lines,
            &mut next_sequence,
        )
        .expect("load test line program");

        assert_eq!(statements.len(), 2);
        assert_eq!(statements[0].address, ImageAddress::new(0x100));
        assert_eq!(statements[0].ordinal, 0);
        assert!(statements[0].location.is_none());
        assert!(statements[0].flags.prologue_end());
        assert_eq!(statements[1].address, ImageAddress::new(0x100));
        assert_eq!(statements[1].ordinal, 1);
        assert_eq!(
            statements[1]
                .location
                .as_ref()
                .map(|location| location.line),
            LineNumber::new(10)
        );
        assert!(statements[1].flags.epilogue_begin());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].range.start, ImageAddress::new(0x100));
        assert_eq!(lines[0].range.end, ImageAddress::new(0x104));
    }

    fn analyzed_entry_row(address: u64, line: u64, sequence: u32, ordinal: u32) -> StatementRow {
        StatementRow {
            address: ImageAddress::new(address),
            operation_index: 0,
            location: Some(SourceLocation {
                file: SourceFileId::new(0),
                line: LineNumber::new(line).expect("nonzero test line"),
                column: None,
            }),
            discriminator: 0,
            flags: StatementFlags::empty().with_statement(true),
            isa: 0,
            sequence: LineSequenceId::new(sequence),
            ordinal,
        }
    }

    #[test]
    fn analyzed_entry_ignores_later_rows_for_the_signature_line() {
        let statements = [
            analyzed_entry_row(0x100, 10, 0, 0),
            analyzed_entry_row(0x110, 10, 0, 1),
            analyzed_entry_row(0x120, 11, 0, 2),
        ];

        assert_eq!(
            first_distinct_source_statement(
                &StatementIndex::new(&statements),
                AddressRange {
                    start: ImageAddress::new(0x100),
                    end: ImageAddress::new(0x130),
                },
                ImageAddress::new(0x100),
            ),
            Some(ImageAddress::new(0x120))
        );
    }

    #[test]
    fn analyzed_entry_stays_within_one_line_program_sequence() {
        // A foreign sequence overlapping the entry address makes attribution
        // ambiguous: no candidate may be derived from mixed sequences.
        let ambiguous = [
            analyzed_entry_row(0x100, 10, 0, 0),
            analyzed_entry_row(0x100, 50, 1, 0),
            analyzed_entry_row(0x120, 11, 0, 1),
        ];
        assert_eq!(
            first_distinct_source_statement(
                &StatementIndex::new(&ambiguous),
                AddressRange {
                    start: ImageAddress::new(0x100),
                    end: ImageAddress::new(0x130),
                },
                ImageAddress::new(0x100),
            ),
            None
        );

        // A foreign sequence that only overlaps the body must not supply the
        // candidate address for the entry's sequence.
        let foreign_candidate = [
            analyzed_entry_row(0x100, 10, 0, 0),
            analyzed_entry_row(0x110, 50, 1, 0),
            analyzed_entry_row(0x120, 11, 0, 1),
        ];
        assert_eq!(
            first_distinct_source_statement(
                &StatementIndex::new(&foreign_candidate),
                AddressRange {
                    start: ImageAddress::new(0x100),
                    end: ImageAddress::new(0x130),
                },
                ImageAddress::new(0x100),
            ),
            Some(ImageAddress::new(0x120))
        );
    }

    impl MemoryReader for TestMemory {
        fn read_u64(&mut self, address: VirtualAddress) -> Option<u64> {
            self.values.get(&address).copied()
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

    #[test]
    fn unwind_expressions_reject_non_default_address_spaces() {
        // DW_OP_lit0, DW_OP_lit1, DW_OP_xderef: dereference address 0 in
        // address space 1. The evaluator must reject the non-default space
        // instead of silently reading the default inferior address space.
        let bytes = [0x30, 0x31, 0x18];
        let section = EhFrame::new(&bytes, RunTimeEndian::Little);
        let expression = UnwindExpression {
            offset: 0usize,
            length: bytes.len(),
        };
        let encoding = Encoding {
            format: Format::Dwarf32,
            version: 4,
            address_size: 8,
        };
        let registers = RegisterFile::new([]);
        let mut memory = TestMemory {
            values: BTreeMap::new(),
        };

        assert_eq!(
            evaluate_unwind_expression(&expression, &section, encoding, &registers, &mut memory),
            Err(UnwindTermination::UnsupportedUnwindInfo {
                feature: "CFA expression: non-default memory address space".into()
            })
        );
    }
}
