use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
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
    CodeInstanceKind, ColumnNumber, EmbeddedSymbolTable, EntryProvenance, Error, FunctionId,
    FunctionInfo, ImageAddress, LineNumber, LineSequenceId, ModuleImage, PointerWidth, Result,
    SourceFile, SourceFileId, SourceLanguage, SourceLocation, StatementFlags, StatementRow,
    TargetDescription, UnwindTermination, VirtualAddress,
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

pub(in crate::debug_info) use variables::{PathStep, array_byte_offset};

#[cfg(feature = "fuzzing")]
pub(super) fn fuzz_expression(data: &[u8]) {
    variables::fuzz_expression(data);
}

struct DwarfUnwindInfo {
    eh_frame: Arc<[u8]>,
    debug_frame: Arc<[u8]>,
    eh_frame_index: FdeIndex,
    debug_frame_index: FdeIndex,
    endian: RunTimeEndian,
    address_size: u8,
    bases: BaseAddresses,
    /// Code the Go toolchain compiled, sorted by start address. Go's calling
    /// convention lets a callee overwrite registers the System V ABI
    /// preserves.
    go_code: Vec<AddressRange<ImageAddress>>,
    /// Go's function table, which unwinds Go code no call-frame information
    /// describes and says where Go frames saved the frame pointer.
    go: Option<Arc<super::gopclntab::GoUnwind>>,
}

pub fn load(
    path: &Path,
    image_id: crate::ModuleImageId,
    search: &super::DebugFileSearch,
) -> Result<DebugInfo> {
    let data: Arc<[u8]> = fs::read(path)?.into();
    load_debug_info(path, &data, image_id, search).map_err(Error::debug_info)
}

pub fn load_bytes(
    path: &Path,
    data: &[u8],
    image_id: crate::ModuleImageId,
    search: &super::DebugFileSearch,
) -> Result<DebugInfo> {
    load_debug_info(path, data, image_id, search).map_err(Error::debug_info)
}

/// Loads an image's debug information. A file without DWARF of its own may
/// have a separate debug file, whose DWARF, symbols, and call-frame
/// information describe the code here. One that cannot be loaded leaves
/// the image as its own file describes it, with the reason recorded.
fn load_debug_info(
    path: &Path,
    data: &[u8],
    image_id: crate::ModuleImageId,
    search: &super::DebugFileSearch,
) -> std::result::Result<DebugInfo, DwarfError> {
    let object = object::File::parse(data)?;
    let Some(separate) = search.find(path, &object) else {
        return load_image(path, data, image_id, Separate::None);
    };
    // dwz moves what several debug files share into a supplementary file,
    // whose units and strings the loader does not read.
    if object::File::parse(separate.data.as_slice())
        .is_ok_and(|debug| debug.section_by_name(".gnu_debugaltlink").is_some())
    {
        let reason = "it shares debug information with other files through a dwz supplementary \
                      file (.gnu_debugaltlink), which uscope does not read";
        return load_image(
            path,
            data,
            image_id,
            Separate::Unusable(&separate.path, reason.into()),
        );
    }
    load_image(path, data, image_id, Separate::Used(&separate)).or_else(|error| {
        load_image(
            path,
            data,
            image_id,
            Separate::Unusable(&separate.path, error.to_string().into()),
        )
    })
}

/// What a separate debug file contributes to an image.
enum Separate<'a> {
    None,
    Used(&'a super::separate::DebugFile),
    /// One found but unusable, for the reason given.
    Unusable(&'a Path, Arc<str>),
}

#[expect(
    clippy::too_many_lines,
    reason = "one loader assembles every table of an image from its sources"
)]
fn load_image(
    path: &Path,
    data: &[u8],
    image_id: crate::ModuleImageId,
    separate: Separate<'_>,
) -> std::result::Result<DebugInfo, DwarfError> {
    let object = object::File::parse(data)?;
    let target = target_description(&object)?;
    let debug_object = match &separate {
        Separate::Used(file) => Some(object::File::parse(file.data.as_slice())?),
        Separate::None | Separate::Unusable(..) => None,
    };
    let dwarf_object = debug_object.as_ref().unwrap_or(&object);

    let sections = DwarfSections::load(
        |id: SectionId| -> std::result::Result<Cow<'_, [u8]>, DwarfError> {
            match dwarf_object.section_by_name(id.name()) {
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

    // Go's own function table, which the runtime reads and stripping keeps.
    let (mut go_table, mut runtime_function_table) = match super::gopclntab::load(&object) {
        Ok(Some(table)) => (Some(Arc::new(table)), EmbeddedSymbolTable::Loaded),
        Ok(None) => (None, EmbeddedSymbolTable::Absent),
        Err(error) => (None, unusable_table(&error)),
    };
    let mut function_metadata = load_function_metadata(
        &dwarf,
        &catalog,
        go_table.as_deref(),
        &mut source_files,
        &mut source_file_ids,
    )?;

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

    // Code no DWARF describes, such as a stripped image's, gets functions
    // and lines from the function table.
    let code = |address: u64, length: usize| {
        code_bytes(&object, address, address.checked_add(length as u64)?)
    };
    if let Some(table) = &go_table
        && let Err(error) = super::gopclntab::complete_metadata(
            table,
            code,
            &mut super::gopclntab::Catalog {
                functions: &mut function_metadata.functions,
                code_instances: &mut function_metadata.code_instances,
                statements: &mut statements,
                lines: &mut lines,
                next_sequence: &mut next_sequence,
                source_file: &mut |path| {
                    source_file_id(path, &mut source_files, &mut source_file_ids)
                },
            },
        )
    {
        runtime_function_table = unusable_table(&error);
        go_table = None;
    }

    super::roles::link_loop_bodies(&mut function_metadata.functions);
    refine_proved_prologue_entries(
        &object,
        target,
        &statements,
        &mut function_metadata.code_instances,
    );

    let mut variables = variables::load_variable_info(
        &dwarf,
        &catalog,
        target,
        image_id,
        variables::CodeMetadata {
            instance_ids: &function_metadata.instance_ids,
            lines: &lines,
            instances: &function_metadata.code_instances,
        },
        &mut source_files,
        &mut source_file_ids,
    )?;
    let coroutines = std::mem::take(&mut variables.coroutines);
    for (instance, ty) in &variables.coroutine_bodies {
        if let Some(Ok(_)) = coroutines.get(ty)
            && let Some(function) = function_metadata
                .code_instances
                .get(instance.index())
                .map(|instance| instance.function)
        {
            function_metadata.functions[function.index()].coroutine = Some(*ty);
        }
    }
    #[cfg(target_arch = "x86_64")]
    let resume_points = if target.architecture == Architecture::X86_64 {
        decode_resume_points(
            &object,
            &statements,
            &function_metadata.functions,
            &mut function_metadata.code_instances,
            &coroutines,
        )
    } else {
        BTreeMap::new()
    };
    #[cfg(target_arch = "x86_64")]
    variables.info.note_held(
        &ObjectCode(&object),
        &function_metadata.code_instances,
        &resume_points,
    );
    #[cfg(not(target_arch = "x86_64"))]
    let resume_points = BTreeMap::new();
    let go_code = go_code_ranges(&dwarf, &catalog)?;
    let go_unwind = go_table
        .as_ref()
        .map(|table| Arc::new(super::gopclntab::GoUnwind::new(Arc::clone(table), code)));
    let unwind = Arc::new(load_unwind_info(
        &object,
        debug_object.as_ref(),
        target,
        go_code,
        go_unwind,
    )?);
    let mut symbols =
        super::elf::load_symbols(&object, debug_object.as_ref(), &unwind.function_ranges());
    symbols.sources.runtime_function_table = runtime_function_table;
    if let Some(table) = &go_table {
        assign_go_symbol_roles(table, &mut symbols.symbols);
    }
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
                got_slots: symbols.got_slots,
                globals: variables.globals,
                types: variables.types,
                vtables: variables.vtables,
                coroutines,
                resume_points,
                constants: variables.constants,
                producers: unit_producers(&dwarf, &catalog)?,
                packages: go_packages(&dwarf, &catalog)?,
                source_files,
                statements,
                lines,
                sections: super::elf::load_sections(&object),
                thread_local_storage: super::elf::has_thread_local_storage(&object),
                thread_locals: super::elf::load_thread_locals(&object),
            },
        )
        .with_id(image_id)
        .with_views(embedded_views(path, dwarf_object)?)
        .with_debug_file(match separate {
            Separate::None => None,
            Separate::Used(file) => Some(crate::DebugFile::Used(Arc::new(file.path.clone()))),
            Separate::Unusable(path, reason) => Some(crate::DebugFile::Unusable {
                path: Arc::new(path.to_path_buf()),
                reason,
            }),
        }),
    );

    Ok(DebugInfo {
        image,
        unwind,
        variables: Arc::new(variables.info),
    })
}

/// The views a module carries for its own types, named after its file.
fn embedded_views(
    path: &Path,
    object: &object::File<'_>,
) -> std::result::Result<Arc<crate::view::ViewSet>, DwarfError> {
    let Some(section) = object.section_by_name(crate::view::embedded::SECTION) else {
        return Ok(crate::view::ViewSet::empty());
    };
    let bytes = section.uncompressed_data()?;
    let module = path
        .file_name()
        .map_or_else(|| "module".into(), |name| name.to_string_lossy());
    Ok(Arc::new(crate::view::embedded::view_set(&module, &bytes)))
}

/// The distinct producers the units name, in the order first named.
fn unit_producers<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    catalog: &UnitCatalog<'data>,
) -> std::result::Result<Vec<Arc<str>>, DwarfError> {
    let mut producers = Vec::<Arc<str>>::new();
    for unit in &catalog.units {
        let mut entries = unit.entries();
        let Some(root) = entries.next_dfs()? else {
            continue;
        };
        if let Some(producer) = string_attribute(dwarf, unit, root, gimli::DW_AT_producer)?
            && !producers.contains(&producer)
        {
            producers.push(producer);
        }
    }
    Ok(producers)
}

/// Go's attribute naming the package a unit compiles, which its
/// `DW_AT_name` names by import path.
const DW_AT_GO_PACKAGE_NAME: gimli::DwAt = gimli::DwAt(0x2905);

/// The Go packages the image has units for, with the names their code
/// declares.
fn go_packages(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    catalog: &UnitCatalog<'_>,
) -> std::result::Result<Vec<crate::model::PackageInfo>, DwarfError> {
    let mut packages = Vec::new();
    for unit in catalog.units.iter().filter(|unit| !is_type_unit(unit)) {
        let mut entries = unit.entries();
        let Some(root) = entries.next_dfs()? else {
            continue;
        };
        if let (Some(path), Some(name)) = (
            string_attribute(dwarf, unit, root, gimli::DW_AT_name)?,
            string_attribute(dwarf, unit, root, DW_AT_GO_PACKAGE_NAME)?,
        ) {
            packages.push(crate::model::PackageInfo { path, name });
        }
    }
    Ok(packages)
}

fn unusable_table(error: &super::gopclntab::PclntabError) -> EmbeddedSymbolTable {
    EmbeddedSymbolTable::Unusable {
        reason: error.to_string().into(),
    }
}

/// Gives each symbol naming a Go function's entry the role the function
/// table records for it. Its name is the table's, without the ELF symbol's
/// ABI suffix.
fn assign_go_symbol_roles(table: &super::gopclntab::GoTable, symbols: &mut [crate::SymbolInfo]) {
    for symbol in symbols {
        let Some(function) = table
            .function_containing(symbol.address.get())
            .filter(|function| function.entry == symbol.address.get() && function.is_go())
        else {
            continue;
        };
        if let Ok(name) = table.name(function) {
            symbol.role = super::roles::go_role(&name, Some(function.facts), false);
        }
    }
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
    debug_object: Option<&object::File<'_>>,
    target: TargetDescription,
    go_code: Vec<AddressRange<ImageAddress>>,
    go: Option<Arc<super::gopclntab::GoUnwind>>,
) -> std::result::Result<DwarfUnwindInfo, DwarfError> {
    let data_in = |object: &object::File<'_>, name| -> std::result::Result<Arc<[u8]>, DwarfError> {
        Ok(object
            .section_by_name(name)
            .as_ref()
            .map(ObjectSection::uncompressed_data)
            .transpose()?
            .unwrap_or_default()
            .into())
    };
    let section_data = |name| data_in(object, name);
    // A separate debug file may hold the `.debug_frame` the module's file
    // was stripped of.
    let debug_frame = match section_data(".debug_frame")? {
        frame if frame.is_empty() => match debug_object {
            Some(debug_object) => data_in(debug_object, ".debug_frame")?,
            None => frame,
        },
        frame => frame,
    };
    let mut bases = BaseAddresses::default();
    if let Some(section) = object.section_by_name(".eh_frame") {
        bases = bases.set_eh_frame(section.address());
    }
    if let Some(section) = object.section_by_name(".text") {
        bases = bases.set_text(section.address());
    }
    if let Some(section) = object.section_by_name(".got") {
        bases = bases.set_got(section.address());
    }

    let mut unwind = DwarfUnwindInfo {
        eh_frame: section_data(".eh_frame")?,
        debug_frame,
        eh_frame_index: FdeIndex::default(),
        debug_frame_index: FdeIndex::default(),
        endian: match target.byte_order {
            ByteOrder::Little => RunTimeEndian::Little,
            ByteOrder::Big => RunTimeEndian::Big,
        },
        address_size: target.pointer_width.bytes(),
        bases,
        go_code,
        go,
    };
    unwind.eh_frame_index = FdeIndex::new(&unwind.eh_frame(), &unwind.bases);
    unwind.debug_frame_index = FdeIndex::new(&unwind.debug_frame(), &unwind.bases);
    Ok(unwind)
}

/// The frame description entries of one call-frame section, indexed once by
/// address so that finding an address's entry is a binary search rather
/// than a walk of the section. Lookups agree exactly with gimli's walk
/// (`UnwindSection::fde_for_address`), which returns the first entry in
/// section order that contains the address, or the first error before it.
#[derive(Debug, Default)]
struct FdeIndex {
    /// Every entry that parses and covers some code, sorted by start.
    entries: Vec<IndexedFde>,
    /// The greatest end among `entries[..=i]`, which bounds how far back a
    /// lookup must look when entries overlap.
    reach: Vec<u64>,
    /// The section offset of the first entry that fails to parse, or
    /// `usize::MAX` when the section itself is malformed and enumeration
    /// stops, with the error. A walk of the section stops there.
    first_error: Option<(usize, gimli::Error)>,
}

#[derive(Debug, Clone, Copy)]
struct IndexedFde {
    start: u64,
    end: u64,
    offset: usize,
}

impl FdeIndex {
    fn new<'data, S>(section: &S, bases: &BaseAddresses) -> Self
    where
        S: UnwindSection<Reader<'data>>,
    {
        let mut index = Self::default();
        let mut entries = section.entries(bases);
        loop {
            let partial = match entries.next() {
                Ok(Some(gimli::CieOrFde::Fde(partial))) => partial,
                Ok(Some(gimli::CieOrFde::Cie(_))) => continue,
                Ok(None) => break,
                Err(error) => {
                    index.first_error.get_or_insert((usize::MAX, error));
                    break;
                }
            };
            match partial.parse(S::cie_from_offset) {
                Ok(fde) if fde.initial_address() < fde.end_address() => {
                    index.entries.push(IndexedFde {
                        start: fde.initial_address(),
                        end: fde.end_address(),
                        offset: fde.offset(),
                    });
                }
                Ok(_) => {}
                Err(error) => {
                    index
                        .first_error
                        .get_or_insert_with(|| (partial.offset(), error));
                }
            }
        }
        index
            .entries
            .sort_unstable_by_key(|fde| (fde.start, fde.offset));
        index.reach = index
            .entries
            .iter()
            .scan(0, |reach, fde| {
                *reach = fde.end.max(*reach);
                Some(*reach)
            })
            .collect();
        index
    }

    /// The section offset of the entry describing `address`.
    fn lookup(&self, address: u64) -> gimli::Result<usize> {
        let after = self.entries.partition_point(|fde| fde.start <= address);
        let first = (0..after)
            .rev()
            .take_while(|&index| self.reach[index] > address)
            .map(|index| self.entries[index])
            .filter(|fde| address < fde.end)
            .map(|fde| fde.offset)
            .min();
        match (first, self.first_error) {
            (Some(offset), Some((error_offset, _))) if offset < error_offset => Ok(offset),
            (Some(offset), None) => Ok(offset),
            (_, Some((_, error))) => Err(error),
            (None, None) => Err(gimli::Error::NoUnwindInfoForAddress),
        }
    }
}

impl DwarfUnwindInfo {
    /// Returns the code range of every function the call-frame information
    /// describes. Enumeration stops at the first malformed entry, so the
    /// result is evidence of function boundaries rather than a complete map.
    fn function_ranges(&self) -> Vec<AddressRange<ImageAddress>> {
        self.eh_frame_index
            .entries
            .iter()
            .chain(&self.debug_frame_index.entries)
            .map(|fde| AddressRange {
                start: ImageAddress::new(fde.start),
                end: ImageAddress::new(fde.end),
            })
            .collect()
    }

    fn eh_frame(&self) -> EhFrame<Reader<'_>> {
        let mut section = EhFrame::new(&self.eh_frame, self.endian);
        section.set_address_size(self.address_size);
        section
    }

    fn debug_frame(&self) -> DebugFrame<Reader<'_>> {
        let mut section = DebugFrame::new(&self.debug_frame, self.endian);
        section.set_address_size(self.address_size);
        section
    }

    /// Returns the registers the function at `address` may overwrite
    /// without saving them, by the calling convention it follows.
    fn call_clobbered_registers(&self, address: ImageAddress) -> &'static [u16] {
        let after = self.go_code.partition_point(|range| range.start <= address);
        if after > 0 && self.go_code[after - 1].contains(address)
            || self.go.as_ref().is_some_and(|go| go.is_go(address.get()))
        {
            &X86_64_GO_CALL_CLOBBERED_REGISTERS
        } else {
            &X86_64_SYSV_CALL_CLOBBERED_REGISTERS
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
        let result = cfa_from_section(
            &self.eh_frame(),
            &self.eh_frame_index,
            &self.bases,
            address,
            registers,
            memory,
        );
        if !matches!(result, Err(UnwindTermination::NoUnwindInfo { .. })) {
            return result;
        }
        let result = cfa_from_section(
            &self.debug_frame(),
            &self.debug_frame_index,
            &self.bases,
            address,
            registers,
            memory,
        );
        if !matches!(result, Err(UnwindTermination::NoUnwindInfo { .. })) {
            return result;
        }
        self.go
            .as_ref()
            .and_then(|go| go.cfa(address.get(), registers))
            .unwrap_or(result)
    }

    fn unwind(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<UnwindStep, UnwindTermination> {
        let clobbered = self.call_clobbered_registers(address);
        let result = unwind_from_section(
            &self.eh_frame(),
            &self.eh_frame_index,
            &self.bases,
            address,
            registers,
            clobbered,
            memory,
        );
        let result = if matches!(result, Err(UnwindTermination::NoUnwindInfo { .. })) {
            unwind_from_section(
                &self.debug_frame(),
                &self.debug_frame_index,
                &self.bases,
                address,
                registers,
                clobbered,
                memory,
            )
        } else {
            result
        };
        let Some(go) = &self.go else {
            return result;
        };
        let result = match result {
            Err(UnwindTermination::NoUnwindInfo { .. }) => go
                .unwind(address.get(), registers, clobbered, memory)
                .unwrap_or(result),
            result => result,
        };
        result.map(|mut step| {
            go.recover_frame_pointer(address.get(), registers, &mut step, memory);
            step
        })
    }
}

fn cfa_from_section<'data, S>(
    section: &S,
    index: &FdeIndex,
    bases: &BaseAddresses,
    address: ImageAddress,
    registers: &RegisterFile,
    memory: &mut dyn MemoryReader,
) -> std::result::Result<VirtualAddress, UnwindTermination>
where
    S: UnwindSection<Reader<'data>>,
{
    let mut context = UnwindContext::new();
    let (fde, row) = unwind_row(section, index, bases, address, &mut context)?;
    cfa_from_rule(row.cfa(), registers, section, fde.cie().encoding(), memory)
}

/// The call-frame row in effect at `address`, and the entry holding it.
fn unwind_row<'data, 'context, S>(
    section: &S,
    index: &FdeIndex,
    bases: &BaseAddresses,
    address: ImageAddress,
    context: &'context mut UnwindContext<usize>,
) -> std::result::Result<
    (
        gimli::FrameDescriptionEntry<Reader<'data>>,
        &'context gimli::UnwindTableRow<usize>,
    ),
    UnwindTermination,
>
where
    S: UnwindSection<Reader<'data>>,
{
    let fde = index
        .lookup(address.get())
        .and_then(|offset| section.fde_from_offset(bases, offset.into(), S::cie_from_offset))
        .map_err(|error| cfi_error(error, address))?;
    let row = fde
        .unwind_info_for_address(section, bases, context, address.get())
        .map_err(|error| cfi_error(error, address))?;
    Ok((fde, row))
}

fn unwind_from_section<'data, S>(
    section: &S,
    index: &FdeIndex,
    bases: &BaseAddresses,
    address: ImageAddress,
    registers: &RegisterFile,
    clobbered: &[u16],
    memory: &mut dyn MemoryReader,
) -> std::result::Result<UnwindStep, UnwindTermination>
where
    S: UnwindSection<Reader<'data>>,
{
    let mut context = UnwindContext::new();
    let (fde, row) = unwind_row(section, index, bases, address, &mut context)?;
    let return_register = fde.cie().return_address_register().0;
    let cfa = cfa_from_rule(row.cfa(), registers, section, fde.cie().encoding(), memory)?;
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

    let rules = RuleContext {
        current: registers,
        cfa,
        section,
        encoding: fde.cie().encoding(),
    };
    for &(register, ref rule) in row.registers() {
        rules.apply(&mut caller, memory, register.0, rule)?;
    }
    caller.set(7, cfa.get());

    if caller.get(return_register).is_none() {
        return Err(UnwindTermination::Complete);
    }

    Ok(UnwindStep {
        registers: caller,
        cfa,
        signal_frame: fde.cie().is_signal_trampoline(),
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
            let value = registers
                .get(register.0)
                .ok_or_else(|| register_unavailable(register.0))?;
            Ok(VirtualAddress::new(
                checked_add(value, *offset).ok_or_else(|| UnwindTermination::InvalidCaller {
                    description: "CFA arithmetic overflow".into(),
                })?,
            ))
        }
        CfaRule::Expression(expression) => {
            match evaluate_unwind_expression(
                expression, section, encoding, registers, memory, None, "CFA",
            )? {
                Evaluated::Address(address) => Ok(VirtualAddress::new(address)),
                Evaluated::Value(_) => Err(UnwindTermination::UnsupportedUnwindInfo {
                    feature: "CFA expression: non-address result".into(),
                }),
            }
        }
    }
}

// Bounds unwind-expression evaluation so a malformed expression with a
// backward branch cannot hang the controller thread.
const MAX_UNWIND_EXPRESSION_ITERATIONS: u32 = 10_000;

/// What an unwind expression computed: an address, or with
/// `DW_OP_stack_value`, a value.
enum Evaluated {
    Address(u64),
    Value(u64),
}

/// Evaluates a CFA or register rule's expression. A register rule's
/// expression starts with the CFA on its stack, as `initial`.
fn evaluate_unwind_expression<'data, S>(
    expression: &UnwindExpression<usize>,
    section: &S,
    encoding: Encoding,
    registers: &RegisterFile,
    memory: &mut dyn MemoryReader,
    initial: Option<u64>,
    rule: &str,
) -> std::result::Result<Evaluated, UnwindTermination>
where
    S: UnwindSection<Reader<'data>>,
{
    let unsupported = |feature: &str| UnwindTermination::UnsupportedUnwindInfo {
        feature: format!("{rule} expression: {feature}").into(),
    };
    let corrupt = |error: gimli::Error| UnwindTermination::CorruptUnwindInfo {
        description: format!("{rule} expression: {error}").into(),
    };
    let expression = expression.get(section).map_err(corrupt)?;
    let mut evaluation = expression.evaluation(encoding);
    evaluation.set_max_iterations(MAX_UNWIND_EXPRESSION_ITERATIONS);
    if let Some(initial) = initial {
        evaluation.set_initial_value(initial);
    }
    let mut result = evaluation.evaluate().map_err(corrupt)?;
    loop {
        result = match result {
            EvaluationResult::Complete => break,
            EvaluationResult::RequiresRegister { register, .. } => {
                let value = registers
                    .get(register.0)
                    .ok_or_else(|| register_unavailable(register.0))?;
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
        Location::Address { address } => Ok(Evaluated::Address(address)),
        Location::Value {
            value: Value::Generic(value),
        } => Ok(Evaluated::Value(value)),
        _ => Err(unsupported(
            "a result that is neither an address nor a word",
        )),
    }
}

/// What a row's register rules are applied with.
struct RuleContext<'a, S> {
    /// The registers of the frame being unwound.
    current: &'a RegisterFile,
    cfa: VirtualAddress,
    section: &'a S,
    encoding: Encoding,
}

impl<'data, S: UnwindSection<Reader<'data>>> RuleContext<'_, S> {
    fn apply(
        &self,
        caller: &mut RegisterFile,
        memory: &mut dyn MemoryReader,
        register: u16,
        rule: &RegisterRule<usize>,
    ) -> std::result::Result<(), UnwindTermination> {
        let cfa = self.cfa;
        let value = match rule {
            RegisterRule::Undefined => {
                caller.remove(register);
                return Ok(());
            }
            RegisterRule::SameValue => self
                .current
                .get(register)
                .ok_or_else(|| register_unavailable(register))?,
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
            RegisterRule::Register(source) => self
                .current
                .get(source.0)
                .ok_or_else(|| register_unavailable(source.0))?,
            RegisterRule::Constant(value) => *value,
            // The register was saved where the expression computes.
            RegisterRule::Expression(expression) => match self.evaluate(expression, memory)? {
                Evaluated::Address(address) => {
                    let address = VirtualAddress::new(address);
                    memory
                        .read_u64(address)
                        .ok_or(UnwindTermination::MemoryReadFailed { address })?
                }
                Evaluated::Value(_) => {
                    return Err(UnwindTermination::CorruptUnwindInfo {
                        description: "register expression: a value where a saved \
                                          register's address belongs"
                            .into(),
                    });
                }
            },
            // The register's value is what the expression computes.
            RegisterRule::ValExpression(expression) => match self.evaluate(expression, memory)? {
                Evaluated::Address(value) | Evaluated::Value(value) => value,
            },
            RegisterRule::Architectural => {
                return Err(UnwindTermination::UnsupportedUnwindInfo {
                    feature: "architectural register rule".into(),
                });
            }
        };
        caller.set(register, value);
        Ok(())
    }

    fn evaluate(
        &self,
        expression: &UnwindExpression<usize>,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<Evaluated, UnwindTermination> {
        evaluate_unwind_expression(
            expression,
            self.section,
            self.encoding,
            self.current,
            memory,
            Some(self.cfa.get()),
            "register",
        )
    }
}

fn register_unavailable(register: u16) -> UnwindTermination {
    UnwindTermination::RegisterUnavailable {
        register: format!("DWARF register {register}").into(),
    }
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
    /// The language of the unit holding the DIE.
    language: SourceLanguage,
    kind: RawFunctionKind,
    parent: Option<DieKey>,
    abstract_origin: Option<DieKey>,
    specification: Option<DieKey>,
    name: Option<Arc<str>>,
    linkage_name: Option<Arc<str>>,
    /// Whether the DIE says the code only forwards to another function.
    trampoline: bool,
    declaration: Option<SourceLocation>,
    call_site: Option<SourceLocation>,
    ranges: Vec<AddressRange<ImageAddress>>,
    entry: Option<ImageAddress>,
    /// The names of the namespaces enclosing the DIE, outermost first,
    /// joined by `::`, as Rust's debug information nests its functions.
    namespace: Option<Arc<str>>,
    /// The type the function returns.
    returns: Option<DieKey>,
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
    go_table: Option<&super::gopclntab::GoTable>,
    source_files: &mut Vec<SourceFile>,
    source_file_ids: &mut HashMap<PathBuf, SourceFileId>,
) -> std::result::Result<FunctionMetadata, DwarfError> {
    let (raw, futures) = collect_function_dies(dwarf, catalog, source_files, source_file_ids)?;
    let by_key: HashMap<_, _> = raw
        .iter()
        .enumerate()
        .map(|(index, function)| (function.key, index))
        .collect();
    let mut functions = Vec::new();
    let mut trampolines = Vec::new();
    let mut function_ids = HashMap::new();
    // The definitions code belongs to. Clang also emits subprograms with
    // no code and no name, only to scope a function's local types.
    let mut concrete = HashSet::new();
    for function in raw.iter().filter(|function| !function.ranges.is_empty()) {
        concrete.insert(definition_key(function.key, &raw, &by_key)?);
    }

    for function in &raw {
        let definition = definition_key(function.key, &raw, &by_key)?;

        if function_ids.contains_key(&definition) {
            continue;
        }
        let origin = &raw[by_key[&definition]];
        let linkage_name = origin.linkage_name.clone();
        // Clang names the thunks a multiply inherited virtual function
        // needs only by their linkage names.
        let name = function_name(origin);
        let Some(name) = name else {
            if concrete.contains(&definition) {
                return Err(DwarfError::MissingFunctionName);
            }
            continue;
        };
        let declaration = origin.declaration.clone();
        let id = FunctionId::new(
            u32::try_from(functions.len()).map_err(|_| gimli::Error::UnsupportedOffset)?,
        );
        let role = super::roles::function_role(
            linkage_name.as_deref().unwrap_or(&name),
            origin.trampoline || builds_future(origin, &futures),
        );
        // What builds a future only wraps, even a runtime's: a step that
        // enters the runtime goes on into the future's body.
        let role = match (&origin.namespace, &origin.name) {
            (Some(namespace), Some(own))
                if origin.language == SourceLanguage::Rust && role != crate::CodeRole::Wrapper =>
            {
                super::roles::rust_role(namespace, own).unwrap_or(role)
            }
            _ => role,
        };
        trampolines.push(origin.trampoline);

        functions.push(FunctionInfo {
            id,
            name,
            linkage_name,
            declaration,
            language: origin.language,
            role,
            enclosing: None,
            coroutine: None,
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
            breakpoint_entry: breakpoint_entry(function),
        });
        instance_ids.insert(function.key, id);
    }

    assign_go_function_roles(
        &mut functions,
        &trampolines,
        source_files,
        &code_instances,
        go_table,
    );
    Ok(FunctionMetadata {
        functions,
        code_instances,
        instance_ids,
    })
}

/// The name a function shows. Clang names the thunks a multiply inherited
/// virtual function needs only by their linkage names, and the body of a
/// Rust `async fn` or block shows as the function its programmer wrote.
fn function_name(function: &RawFunction) -> Option<Arc<str>> {
    let name = function.name.clone().or_else(|| {
        function
            .linkage_name
            .as_deref()
            .and_then(crate::demangle::demangle)
            .map(Arc::from)
    });
    match (&name, &function.namespace) {
        (Some(raw), Some(namespace)) if function.language == SourceLanguage::Rust => {
            let path = namespace.split("::").map(Arc::from).collect::<Vec<_>>();
            super::coroutines::body_name(raw, &path).or(name)
        }
        _ => name,
    }
}

/// Whether a function is an `async fn` as rustc compiles it apart from its
/// body: code that only builds the future, which returns the coroutine of
/// the `async fn` of its own name, less a generic function's arguments.
/// `futures` names the function each `async fn`'s coroutine type belongs
/// to.
fn builds_future(function: &RawFunction, futures: &Futures) -> bool {
    function.language == SourceLanguage::Rust
        && function
            .returns
            .and_then(|returns| futures.get(&returns))
            .zip(
                function
                    .name
                    .as_deref()
                    .and_then(super::coroutines::without_arguments),
            )
            .is_some_and(|(of, own)| **of == *own)
}

/// Where a breakpoint on a code instance goes: the entry its DIE names,
/// when that lies in its code, or else where its code begins.
fn breakpoint_entry(function: &RawFunction) -> Option<BreakpointEntry> {
    function
        .entry
        .filter(|entry| function.ranges.iter().any(|range| range.contains(*entry)))
        .map(|address| BreakpointEntry {
            address,
            provenance: EntryProvenance::Explicit,
        })
        .or_else(|| {
            function.ranges.first().map(|range| BreakpointEntry {
                address: range.start,
                provenance: EntryProvenance::RangeStart,
            })
        })
}

/// Gives each Go function its role, from its name, whether the compiler
/// generated it as a trampoline or an ABI wrapper, and what the function
/// table records at its entry. An ABI wrapper shares its function's DWARF
/// name but has its own entry.
fn assign_go_function_roles(
    functions: &mut [FunctionInfo],
    trampolines: &[bool],
    source_files: &[SourceFile],
    instances: &[CodeInstanceInfo],
    go_table: Option<&super::gopclntab::GoTable>,
) {
    let cgo = super::roles::cgo_generated(functions, source_files);
    let runtime_c = super::roles::cgo_runtime(functions, source_files);
    let generated = super::roles::abi_wrappers(functions, source_files)
        .into_iter()
        .zip(trampolines)
        .zip(&cgo)
        .map(|((abi_wrapper, trampoline), cgo)| abi_wrapper || *trampoline || *cgo)
        .collect::<Vec<_>>();
    let mut facts = vec![None; functions.len()];
    if let Some(table) = go_table {
        for instance in instances
            .iter()
            .filter(|instance| matches!(instance.kind, CodeInstanceKind::OutOfLine))
        {
            let Some(entry) = instance.ranges.first().map(|range| range.start.get()) else {
                continue;
            };
            facts[instance.function.index()] = table
                .function_containing(entry)
                .filter(|function| function.entry == entry)
                .map(|function| function.facts);
        }
    }
    for ((function, (facts, generated)), runtime_c) in functions
        .iter_mut()
        .zip(facts.into_iter().zip(generated))
        .zip(runtime_c)
    {
        if function.language == SourceLanguage::Go {
            function.role = super::roles::go_role(&function.name, facts, generated);
        } else if function.role == crate::CodeRole::Ordinary {
            if generated {
                // cgo's C, such as the code that Go's calls to C enter.
                function.role = crate::CodeRole::Wrapper;
            } else if runtime_c && go_table.is_some() {
                function.role = crate::CodeRole::RuntimeInternal;
            }
        }
    }
}

fn collect_function_dies(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    catalog: &UnitCatalog<'_>,
    source_files: &mut Vec<SourceFile>,
    source_file_ids: &mut HashMap<PathBuf, SourceFileId>,
) -> std::result::Result<(Vec<RawFunction>, Futures), DwarfError> {
    let units = catalog.units.as_slice();
    let mut functions = Vec::new();
    let mut futures = Futures::new();

    for (unit_index, unit) in units.iter().enumerate() {
        if is_type_unit(unit) {
            continue;
        }
        let language = unit_language(dwarf, unit)?;
        let mut entries = unit.entries();
        let mut scopes = Vec::<Option<DieKey>>::new();
        // The namespace path each depth is within.
        let mut namespaces = Vec::<Option<Arc<str>>>::new();

        while let Some(entry) = entries.next_dfs()? {
            let depth =
                usize::try_from(entry.depth()).map_err(|_| DwarfError::InvalidEntryDepth)?;
            scopes.truncate(depth);
            namespaces.truncate(depth);
            let parent = scopes.iter().rev().find_map(|key| *key);
            let namespace = namespaces.last().cloned().flatten();
            namespaces.push(namespace_within(dwarf, unit, entry, namespace.as_ref())?);
            let kind = match entry.tag() {
                gimli::DW_TAG_subprogram => Some(RawFunctionKind::Subprogram),
                gimli::DW_TAG_inlined_subroutine => Some(RawFunctionKind::Inline),
                _ => None,
            };
            let key = DieKey {
                unit: unit_index,
                offset: entry.offset().0,
            };
            if language == SourceLanguage::Rust
                && let Some(function) = future_of(dwarf, unit, entry, namespace.as_deref())?
            {
                futures.insert(key, function);
            }

            if let Some(kind) = kind {
                let concrete_ranges = die_code_ranges(dwarf, unit, entry, &catalog.code)?;
                functions.push(RawFunction {
                    key,
                    language,
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
                    name: string_attribute(dwarf, unit, entry, gimli::DW_AT_name)?,
                    linkage_name: string_attribute(dwarf, unit, entry, gimli::DW_AT_linkage_name)?,
                    trampoline: entry
                        .attr_value(gimli::DW_AT_trampoline)
                        .is_some_and(|value| value != gimli::AttributeValue::Flag(false)),
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
                    namespace,
                    returns: die_reference(entry.attr_value(gimli::DW_AT_type), unit_index, units)?,
                });
                scopes.push(Some(key));
            } else {
                scopes.push(None);
            }
        }
    }

    Ok((functions, futures))
}

/// The namespace path a DIE's children are within: its own name appended
/// to `namespace` when it is a namespace.
fn namespace_within(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    namespace: Option<&Arc<str>>,
) -> std::result::Result<Option<Arc<str>>, DwarfError> {
    if entry.tag() != gimli::DW_TAG_namespace {
        return Ok(namespace.cloned());
    }
    Ok(
        string_attribute(dwarf, unit, entry, gimli::DW_AT_name)?.map(|name| {
            namespace.map_or_else(
                || Arc::clone(&name),
                |path| format!("{path}::{name}").into(),
            )
        }),
    )
}

/// The coroutine type of each `async fn`, and that function's name.
type Futures = HashMap<DieKey, Arc<str>>;

/// The name of the `async fn` whose coroutine type a DIE is, which rustc
/// nests in the function's namespace.
fn future_of(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    namespace: Option<&str>,
) -> std::result::Result<Option<Arc<str>>, DwarfError> {
    if entry.tag() != gimli::DW_TAG_structure_type {
        return Ok(None);
    }
    let name = string_attribute(dwarf, unit, entry, gimli::DW_AT_name)?;
    Ok((name.as_deref().and_then(super::coroutines::coroutine_kind)
        == Some(crate::CoroutineKind::AsyncFunction))
    .then(|| namespace.and_then(|namespace| namespace.rsplit("::").next()))
    .flatten()
    .map(Arc::from))
}

/// The language a unit is written in, by its root DIE.
fn unit_language(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
) -> std::result::Result<SourceLanguage, DwarfError> {
    let mut entries = unit.entries();
    let Some(root) = entries.next_dfs()? else {
        return Ok(SourceLanguage::Unknown);
    };
    let language = match root.attr_value(gimli::DW_AT_language) {
        Some(gimli::AttributeValue::Language(language)) => Some(language),
        _ => None,
    };
    let zig = string_attribute(dwarf, unit, root, gimli::DW_AT_producer)?
        .is_some_and(|producer| producer.starts_with("zig "));
    Ok(variables::source_language(language, zig))
}

fn string_attribute(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    attribute: gimli::DwAt,
) -> std::result::Result<Option<Arc<str>>, DwarfError> {
    entry
        .attr_value(attribute)
        .map(|value| dwarf.attr_string(unit, value))
        .transpose()
        .map_err(DwarfError::from)
        .map(|value| value.map(|value| Arc::<str>::from(value.to_string_lossy().as_ref())))
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
                .with_prologue_end(row.prologue_end())
                .with_epilogue_begin(row.epilogue_begin());
            let row_ordinal = ordinal;
            ordinal = ordinal
                .checked_add(1)
                .ok_or(gimli::Error::UnsupportedOffset)?;

            // A row without a file or with line 0 is compiler-generated code.
            // It still ends the previous entry's range, so the gap is not
            // attributed to a neighboring line, and keeps its prologue and
            // epilogue markers.
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
    let id = SourceFileId::new(
        u32::try_from(source_files.len()).expect("source file count fits in u32"),
    );
    source_files.push(SourceFile {
        id,
        path: Arc::new(path.clone()),
    });
    source_file_ids.insert(path, id);
    id
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

/// The bytes of an object file a coroutine's dispatch is decoded from.
#[cfg(target_arch = "x86_64")]
struct ObjectCode<'a, 'data>(&'a object::File<'data>);

#[cfg(target_arch = "x86_64")]
impl ObjectCode<'_, '_> {
    fn section_bytes(&self, address: u64, kinds: &[object::SectionKind]) -> Option<(&[u8], usize)> {
        let section = self.0.sections().find(|section| {
            kinds.contains(&section.kind())
                && section.address() <= address
                && address < section.address().saturating_add(section.size())
        })?;
        let data = section.data().ok()?;
        Some((data, usize::try_from(address - section.address()).ok()?))
    }
}

#[cfg(target_arch = "x86_64")]
impl super::dispatch::DispatchImage for ObjectCode<'_, '_> {
    fn code(&self, address: u64, length: usize) -> Option<&[u8]> {
        let (data, at) = self.section_bytes(address, &[object::SectionKind::Text])?;
        let bytes = data.get(at..)?;
        Some(&bytes[..bytes.len().min(length)])
    }

    fn data(&self, address: u64, length: usize) -> Option<&[u8]> {
        let (data, at) = self.section_bytes(
            address,
            &[
                object::SectionKind::ReadOnlyData,
                object::SectionKind::ReadOnlyString,
                object::SectionKind::Text,
            ],
        )?;
        data.get(at..at.checked_add(length)?)
    }
}

/// Decodes where each out-of-line function that runs a coroutine goes for
/// each state, and moves the breakpoint entry of every instance of one past
/// what leads into its body.
#[cfg(target_arch = "x86_64")]
fn decode_resume_points(
    object: &object::File<'_>,
    statements: &[StatementRow],
    functions: &[FunctionInfo],
    instances: &mut [CodeInstanceInfo],
    coroutines: &BTreeMap<crate::TypeId, std::result::Result<crate::CoroutineInfo, Arc<str>>>,
) -> BTreeMap<CodeInstanceId, std::result::Result<crate::ResumePoints, Arc<str>>> {
    let image = ObjectCode(object);
    let rows = StatementIndex::new(statements);
    let mut decoded = BTreeMap::new();
    for instance in instances.iter_mut() {
        let function = &functions[instance.function.index()];
        let Some(Ok(coroutine)) = function.coroutine.and_then(|ty| coroutines.get(&ty)) else {
            continue;
        };
        let header = function.declaration.as_ref().map(|location| location.line);
        let ranges = Arc::clone(&instance.ranges);
        let in_function = |address: u64| {
            let address = ImageAddress::new(address);
            ranges.iter().any(|range| range.contains(address))
        };
        // What leads into the body before its first statement carries the
        // header's line, which the dispatch and the argument moves have,
        // or none.
        let leading = |address: u64| {
            in_function(address)
                && rows
                    .line_at(ImageAddress::new(address))
                    .is_none_or(|line| line.get() == 0 || Some(line) == header)
        };
        let body_after = |start: ImageAddress| {
            super::dispatch::first_beyond(&image, start, &leading)
                .filter(|body| in_function(body.get()))
        };
        if !matches!(instance.kind, CodeInstanceKind::OutOfLine) {
            // A body inlined into its awaiter is entered straight from the
            // awaiter's code.
            if let Some(body) = instance
                .breakpoint_entry
                .and_then(|entry| body_after(entry.address))
            {
                instance.breakpoint_entry = Some(BreakpointEntry {
                    address: body,
                    provenance: EntryProvenance::CoroutineBody,
                });
            }
            continue;
        }
        let Some(entry) = ranges.iter().map(|range| range.start).min() else {
            continue;
        };
        let states = coroutine
            .states
            .iter()
            .map(|state| state.value)
            .collect::<Vec<_>>();
        let points = super::dispatch::decode(&image, &ranges, entry, coroutine.state, &states).map(
            |mut points| {
                // The code a first poll can run. Resuming runs code of its
                // own until it joins that, such as the rest of a line
                // whose awaited call is inlined.
                let arrival = points
                    .points
                    .iter()
                    .find(|point| {
                        coroutine
                            .state(point.state)
                            .is_some_and(|state| state.kind == crate::CoroutineStateKind::Unresumed)
                    })
                    .map(|point| super::dispatch::flood_all(&image, point.address, &in_function))
                    .filter(|(_, complete)| *complete)
                    .map(|(code, _)| code);
                let mut moved = points.points.to_vec();
                for point in &mut moved {
                    let Some(state) = coroutine.state(point.state) else {
                        continue;
                    };
                    if state.kind == crate::CoroutineStateKind::Unresumed {
                        let body = body_after(point.address).unwrap_or(point.address);
                        point.resumption = [AddressRange {
                            start: point.address,
                            end: body.max(point.address),
                        }]
                        .into();
                        instance.breakpoint_entry = Some(BreakpointEntry {
                            address: body,
                            provenance: EntryProvenance::CoroutineBody,
                        });
                        continue;
                    }
                    // Resuming runs the code of the state's own line, of the
                    // function's header, and of no line, which includes the
                    // loop polling the awaited future that arriving at the
                    // await runs too, and so is not where the await's line
                    // begins. It also runs code no first poll runs before
                    // it joins one's path, such as the rest of a line whose
                    // awaited body is inlined.
                    let own = state.location.as_ref().map(|location| location.line);
                    let within = |address: u64| {
                        in_function(address)
                            && (rows.line_at(ImageAddress::new(address)).is_none_or(|line| {
                                line.get() == 0 || Some(line) == own || Some(line) == header
                            }) || arrival.as_ref().is_some_and(|arrival| {
                                let address = ImageAddress::new(address);
                                !arrival.iter().any(|range| range.contains(address))
                            }))
                    };
                    point.resumption = super::dispatch::flood(&image, point.address, &within);
                }
                points.points = moved.into();
                points
            },
        );
        decoded.insert(instance.id, points);
    }
    decoded
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

    /// The line of the row whose code holds `address`: the last row at or
    /// before it, unless that row has no line.
    fn line_at(&self, address: ImageAddress) -> Option<LineNumber> {
        let after = self.0.partition_point(|row| row.address <= address);
        let row = self.0.get(after.checked_sub(1)?)?;
        row.location.as_ref().map(|location| location.line)
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
    fn analyzed_entry_skips_the_signature_line_within_one_sequence() {
        let row = analyzed_entry_row;
        for (rows, expected) in [
            (
                [
                    row(0x100, 10, 0, 0),
                    row(0x110, 10, 0, 1),
                    row(0x120, 11, 0, 2),
                ],
                Some(0x120),
            ),
            // A foreign sequence at the entry address makes attribution
            // ambiguous.
            (
                [
                    row(0x100, 10, 0, 0),
                    row(0x100, 50, 1, 0),
                    row(0x120, 11, 0, 1),
                ],
                None,
            ),
            // A foreign sequence within the body supplies no candidate.
            (
                [
                    row(0x100, 10, 0, 0),
                    row(0x110, 50, 1, 0),
                    row(0x120, 11, 0, 1),
                ],
                Some(0x120),
            ),
        ] {
            assert_eq!(
                first_distinct_source_statement(
                    &StatementIndex::new(&rows),
                    AddressRange {
                        start: ImageAddress::new(0x100),
                        end: ImageAddress::new(0x130),
                    },
                    ImageAddress::new(0x100),
                ),
                expected.map(ImageAddress::new)
            );
        }
    }

    impl MemoryReader for TestMemory {
        fn read_u64(&mut self, address: VirtualAddress) -> Option<u64> {
            self.values.get(&address).copied()
        }
    }

    const TEST_ENCODING: Encoding = Encoding {
        format: Format::Dwarf32,
        version: 4,
        address_size: 8,
    };

    #[test]
    fn register_rules_distinguish_locations_values_and_frozen_registers() {
        let current = RegisterFile::new([(1, 100), (2, 200), (3, 300)]);
        let mut caller = current.clone();
        let mut memory = TestMemory {
            values: [(0xff8, 0xfeed), (0x64, 0xbeef)]
                .into_iter()
                .map(|(address, value)| (VirtualAddress::new(address), value))
                .collect(),
        };
        // Each expression starts with the CFA on its stack.
        let expressions: [&[u8]; 5] = [
            &[0x38, 0x1c],       // DW_OP_lit8, DW_OP_minus: saved at CFA - 8
            &[0x40, 0x22],       // DW_OP_lit16, DW_OP_plus: CFA + 16 is the value
            &[0x31, 0x22, 0x9f], // DW_OP_lit1, DW_OP_plus, DW_OP_stack_value
            &[0x71, 0x00],       // DW_OP_breg1 0: saved where register 1 points
            &[0x9f],             // DW_OP_stack_value: no address at all
        ];
        let bytes = expressions.concat();
        let section = EhFrame::new(&bytes, RunTimeEndian::Little);
        let mut offset = 0;
        let [below, above, stack_value, through_register, malformed] = expressions.map(|bytes| {
            let expression = UnwindExpression {
                offset,
                length: bytes.len(),
            };
            offset += bytes.len();
            expression
        });
        let mut apply = |caller: &mut RegisterFile, cfa, register, rule| {
            RuleContext {
                current: &current,
                cfa: VirtualAddress::new(cfa),
                section: &section,
                encoding: TEST_ENCODING,
            }
            .apply(caller, &mut memory, register, &rule)
        };
        for (register, rule) in [
            (1, RegisterRule::Constant(999)),
            (4, RegisterRule::Register(Register(1))),
            (5, RegisterRule::Offset(-8)),
            (6, RegisterRule::ValOffset(-8)),
            (3, RegisterRule::Undefined),
            (7, RegisterRule::Expression(below)),
            (8, RegisterRule::ValExpression(above)),
            (9, RegisterRule::ValExpression(stack_value)),
            (10, RegisterRule::Expression(through_register)),
        ] {
            apply(&mut caller, 0x1000, register, rule).unwrap();
        }
        assert_eq!(caller.get(1), Some(999));
        assert_eq!(caller.get(4), Some(100), "rule read mutated caller state");
        assert_eq!(caller.get(5), Some(0xfeed));
        assert_eq!(caller.get(6), Some(0xff8));
        assert_eq!(caller.get(3), None);
        assert_eq!(caller.get(7), Some(0xfeed));
        assert_eq!(caller.get(8), Some(0x1010));
        assert_eq!(caller.get(9), Some(0x1001));
        assert_eq!(
            caller.get(10),
            Some(0xbeef),
            "expressions read the callee's registers"
        );
        assert_eq!(
            apply(&mut caller, 0x1000, 11, RegisterRule::Expression(malformed)),
            Err(UnwindTermination::CorruptUnwindInfo {
                description: "register expression: a value where a saved register's address \
                              belongs"
                    .into(),
            })
        );

        for (cfa, rule, failure) in [
            (
                0,
                RegisterRule::Offset(-1),
                UnwindTermination::InvalidCaller {
                    description: "saved-register address overflow".into(),
                },
            ),
            (
                0x1000,
                RegisterRule::Offset(0),
                UnwindTermination::MemoryReadFailed {
                    address: VirtualAddress::new(0x1000),
                },
            ),
            (
                0,
                RegisterRule::Register(Register(9)),
                UnwindTermination::RegisterUnavailable {
                    register: "DWARF register 9".into(),
                },
            ),
        ] {
            assert_eq!(apply(&mut caller, cfa, 1, rule), Err(failure));
        }
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
        let registers = RegisterFile::new([]);
        let mut memory = TestMemory {
            values: BTreeMap::new(),
        };

        assert!(matches!(
            evaluate_unwind_expression(
                &expression,
                &section,
                TEST_ENCODING,
                &registers,
                &mut memory,
                None,
                "CFA",
            ),
            Err(UnwindTermination::UnsupportedUnwindInfo { feature })
                if &*feature == "CFA expression: non-default memory address space"
        ));
    }

    /// Checks that looking an address up in an [`FdeIndex`] finds the entry
    /// gimli's walk of the whole section finds, or fails as it does, at
    /// every entry's edges and at `extra`.
    fn check_fde_index<'data, S: UnwindSection<Reader<'data>>>(
        section: &S,
        bases: &BaseAddresses,
        index: &FdeIndex,
        extra: &[u64],
    ) {
        let mut probes = vec![0, u64::MAX];
        probes.extend(extra);
        for fde in &index.entries {
            probes.extend([fde.start.wrapping_sub(1), fde.start, fde.start + 1]);
            probes.extend([fde.end - 1, fde.end]);
        }
        for address in probes {
            let walk = section
                .fde_for_address(bases, address, S::cie_from_offset)
                .map(|fde| fde.offset());
            assert_eq!(index.lookup(address), walk, "address {address:#x}");
        }
    }

    /// FDE lookups in real images agree with a walk of the section, and so
    /// do lookups in a section cut short mid-entry.
    #[test]
    fn indexed_fde_lookups_match_a_walk_of_real_sections() {
        let mut indexed = 0;
        let mut truncations = 0;
        for fixture in ["basic", "containers-cpp-gcc-o2", "callers-go"] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("build/test-programs")
                .join(fixture);
            let data = fs::read(&path).expect("run `just build-test-programs`");
            let object = object::File::parse(&*data).expect("ELF");
            let target = target_description(&object).expect("target");
            let unwind = load_unwind_info(&object, None, target, Vec::new(), None).expect("CFI");
            check_fde_index(
                &unwind.eh_frame(),
                &unwind.bases,
                &unwind.eh_frame_index,
                &[],
            );
            check_fde_index(
                &unwind.debug_frame(),
                &unwind.bases,
                &unwind.debug_frame_index,
                &[],
            );
            indexed += unwind.eh_frame_index.entries.len() + unwind.debug_frame_index.entries.len();

            // Cut short mid-entry, a section's walk fails where it ends.
            let probes: Vec<u64> = unwind
                .function_ranges()
                .iter()
                .map(|range| range.start.get())
                .collect();
            let mut eh_frame = EhFrame::new(
                &unwind.eh_frame[..unwind.eh_frame.len() / 2],
                RunTimeEndian::Little,
            );
            eh_frame.set_address_size(8);
            let debug_frame = DebugFrame::new(
                &unwind.debug_frame[..unwind.debug_frame.len() / 2],
                RunTimeEndian::Little,
            );
            let index = FdeIndex::new(&eh_frame, &unwind.bases);
            truncations += usize::from(index.first_error.is_some());
            check_fde_index(&eh_frame, &unwind.bases, &index, &probes);
            let index = FdeIndex::new(&debug_frame, &unwind.bases);
            truncations += usize::from(index.first_error.is_some());
            check_fde_index(&debug_frame, &unwind.bases, &index, &probes);
        }
        assert!(indexed > 1000, "the fixtures describe {indexed} functions");
        assert!(
            truncations >= 2,
            "only {truncations} sections ended mid-entry"
        );
    }

    /// Where entries overlap, the first in section order wins, and an entry
    /// that does not parse hides every entry after it, as in a walk.
    #[test]
    fn indexed_fde_lookups_follow_section_order() {
        // Overlapping entries, in an order the section's walk must respect.
        let mut table = gimli::write::FrameTable::default();
        let cie = table.add_cie(gimli::write::CommonInformationEntry::new(
            Encoding {
                format: Format::Dwarf32,
                version: 1,
                address_size: 8,
            },
            1,
            -8,
            Register(16),
        ));
        for (start, length) in [
            (0x1000, 0x100),
            (0x1080, 0x180),
            (0x0f00, 0x1100),
            (0x1100, 0),
            (0x1040, 0x10),
            (0x3000, 0x100),
        ] {
            table.add_fde(
                cie,
                gimli::write::FrameDescriptionEntry::new(Address::Constant(start), length),
            );
        }
        let mut written = gimli::write::DebugFrame(EndianVec::new(LittleEndian));
        table.write_debug_frame(&mut written).expect("write");
        let mut bytes = written.0.into_vec();
        let bases = BaseAddresses::default();
        let section = DebugFrame::new(&bytes, RunTimeEndian::Little);
        let index = FdeIndex::new(&section, &bases);
        assert_eq!(index.entries.len(), 5);
        check_fde_index(&section, &bases, &index, &[0x1050, 0x1150, 0x1fff]);

        // An entry whose CIE pointer leads nowhere stops the walk there, so
        // later entries never match.
        let mut offsets: Vec<usize> = index.entries.iter().map(|fde| fde.offset).collect();
        offsets.sort_unstable();
        let second = offsets[1];
        bytes[second + 4..second + 8].copy_from_slice(&0x7fff_0000_u32.to_le_bytes());
        let section = DebugFrame::new(&bytes, RunTimeEndian::Little);
        let index = FdeIndex::new(&section, &bases);
        assert!(index.first_error.is_some());
        check_fde_index(&section, &bases, &index, &[0x1050, 0x1150, 0x1fff, 0x3050]);
    }

    /// Loading a large real program, gofmt, does work in proportion to its
    /// debug information, counted as what the loading thread allocates: a
    /// regression bound that a loader doing far more than it did fails.
    /// Loading allocates about 100 bytes, in 0.47 blocks, for each byte of
    /// gofmt's; the bounds are half again as much.
    #[test]
    fn loading_a_large_program_allocates_in_proportion_to_its_debug_information() {
        use crate::test_memory::memory_cap::allocated;
        use object::{Object, ObjectSection};

        for fixture in ["gofmt-go-o0", "gofmt-go-o2"] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("build/test-programs")
                .join(fixture);
            let data = fs::read(&path).expect("run `just build-test-programs`");
            let object = object::File::parse(&*data).expect("ELF");
            let debug = object
                .sections()
                .filter(|section| section.name().is_ok_and(|name| name.starts_with(".debug_")))
                .map(|section| section.uncompressed_data().expect("a section").len() as u64)
                .sum::<u64>();
            let before = allocated();
            let info = load_bytes(
                &path,
                &data,
                crate::ModuleImageId::new(0),
                &super::super::DebugFileSearch::default(),
            )
            .expect("load");
            let after = allocated();
            let blocks = after.blocks - before.blocks;
            let bytes = after.bytes - before.bytes;
            assert!(info.image.functions().len() > 4000, "{fixture}");
            let work = format!("{fixture}: {debug} debug bytes: {blocks} blocks, {bytes} bytes");
            assert!(blocks <= debug * 7 / 10, "{work}");
            assert!(bytes <= debug * 150, "{work}");
        }
    }
}
