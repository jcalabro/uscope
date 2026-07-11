use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gimli::{
    BaseAddresses, CfaRule, DwarfSections, EhFrame, EndianSlice, RegisterRule, RunTimeEndian,
    SectionId, UnwindContext, UnwindSection,
};
use object::{Object, ObjectSection, ObjectSegment, ObjectSymbol};

use super::{DebugInfo, UnwindInfo};
use crate::model::LineEntry;
use crate::unwind::{MemoryReader, RegisterFile, UnwindStep};
use crate::{
    AddressRange, Architecture, ByteOrder, Error, FunctionId, FunctionInfo, ImageAddress,
    LineNumber, ModuleImage, PointerWidth, Result, SourceFile, SourceFileId, SourceLocation,
    SymbolId, SymbolInfo, TargetDescription, UnwindTermination, VirtualAddress,
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

    let image = Arc::new(ModuleImage::new(
        path.to_owned(),
        target,
        image_address_range(&object)?,
        functions,
        load_symbols(&object),
        source_files,
        lines,
    ));
    let unwind = Arc::new(load_unwind_info(&object, target)?);

    Ok(DebugInfo { image, unwind })
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
        let cfa = match row.cfa() {
            CfaRule::RegisterAndOffset { register, offset } => {
                let value = registers.get(register.0).ok_or_else(|| {
                    UnwindTermination::RegisterUnavailable {
                        register: format!("DWARF register {}", register.0).into(),
                    }
                })?;
                VirtualAddress::new(checked_add(value, *offset).ok_or_else(|| {
                    UnwindTermination::InvalidCaller {
                        description: "CFA arithmetic overflow".into(),
                    }
                })?)
            }
            CfaRule::Expression(_) => {
                return Err(UnwindTermination::UnsupportedUnwindInfo {
                    feature: "CFA expression".into(),
                });
            }
        };
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
    source_file_ids: &mut HashMap<PathBuf, SourceFileId>,
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
            let path = source_path(dwarf, unit, header, file)?;
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
