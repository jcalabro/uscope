use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gimli::{DwarfSections, EndianSlice, RunTimeEndian, SectionId};
use object::{Object, ObjectSection, ObjectSymbol};

use crate::model::LineEntry;
use crate::{
    AddressRange, Architecture, ByteOrder, Error, FunctionId, FunctionInfo, ImageAddress,
    LineNumber, ModuleImage, PointerWidth, Result, SourceFile, SourceFileId, SourceLocation,
    SymbolId, SymbolInfo, TargetDescription,
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
}

type Reader<'data> = EndianSlice<'data, RunTimeEndian>;

pub fn load(path: &Path) -> Result<Arc<ModuleImage>> {
    load_image(path).map(Arc::new).map_err(Error::debug_info)
}

fn load_image(path: &Path) -> std::result::Result<ModuleImage, DwarfError> {
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
    let mut functions = Vec::new();
    let mut source_files = Vec::new();
    let mut source_file_ids = HashMap::new();
    let mut lines = Vec::new();
    let mut units = dwarf.units();

    while let Some(header) = units.next()? {
        let unit = dwarf.unit(header)?;
        load_functions(&dwarf, &unit, &mut functions)?;
        load_lines(
            &dwarf,
            &unit,
            &mut source_files,
            &mut source_file_ids,
            &mut lines,
        )?;
    }

    Ok(ModuleImage::new(
        path.to_owned(),
        target,
        functions,
        load_symbols(&object),
        source_files,
        lines,
    ))
}

fn load_functions(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
    functions: &mut Vec<FunctionInfo>,
) -> std::result::Result<(), DwarfError> {
    let mut entries = unit.entries();

    while let Some(entry) = entries.next_dfs()? {
        if entry.tag() != gimli::DW_TAG_subprogram {
            continue;
        }
        let Some(name) = entry.attr(gimli::DW_AT_name) else {
            continue;
        };
        let name: Arc<str> = dwarf
            .attr_string(unit, name.value())?
            .to_string_lossy()
            .into_owned()
            .into();
        let mut ranges = dwarf.die_ranges(unit, entry)?;
        let mut function_ranges = Vec::new();

        while let Some(range) = ranges.next()? {
            function_ranges.push(AddressRange {
                start: ImageAddress::new(range.begin),
                end: ImageAddress::new(range.end),
            });
        }
        if function_ranges.is_empty() {
            continue;
        }

        let linkage_name = entry
            .attr(gimli::DW_AT_linkage_name)
            .map(|attribute| dwarf.attr_string(unit, attribute.value()))
            .transpose()?
            .map(|name| Arc::<str>::from(name.to_string_lossy().into_owned()));
        let id = FunctionId::new(
            u32::try_from(functions.len()).map_err(|_| gimli::Error::UnsupportedOffset)?,
        );

        functions.push(FunctionInfo {
            id,
            name,
            linkage_name,
            ranges: function_ranges.into(),
            declaration: None,
        });
    }

    Ok(())
}

fn load_lines(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
    source_files: &mut Vec<SourceFile>,
    source_file_ids: &mut HashMap<String, SourceFileId>,
    lines: &mut Vec<LineEntry>,
) -> std::result::Result<(), DwarfError> {
    let Some(program) = unit.line_program.clone() else {
        return Ok(());
    };
    let (program, sequences) = program.sequences()?;

    for sequence in sequences {
        let mut rows = program.resume_from(&sequence);
        let mut previous: Option<(u64, SourceLocation)> = None;

        while let Some((header, row)) = rows.next_row()? {
            if row.end_sequence() {
                push_line_range(&mut previous, row.address(), lines);
                continue;
            }
            let Some(file) = row.file(header) else {
                continue;
            };
            let path = dwarf
                .attr_string(unit, file.path_name())?
                .to_string_lossy()
                .into_owned();
            let file_id = source_file_id(path, source_files, source_file_ids);
            let Some(line) = row.line().and_then(|line| LineNumber::new(line.get())) else {
                continue;
            };
            let location = SourceLocation {
                file: file_id,
                line,
                column: None,
            };

            push_line_range(&mut previous, row.address(), lines);
            previous = Some((row.address(), location));
        }
    }

    Ok(())
}

fn source_file_id(
    path: String,
    source_files: &mut Vec<SourceFile>,
    source_file_ids: &mut HashMap<String, SourceFileId>,
) -> SourceFileId {
    *source_file_ids.entry(path.clone()).or_insert_with(|| {
        let id = SourceFileId::new(
            u32::try_from(source_files.len()).expect("source file count fits in u32"),
        );
        source_files.push(SourceFile {
            id,
            path: Arc::new(PathBuf::from(path)),
        });
        id
    })
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
