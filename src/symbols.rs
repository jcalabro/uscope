use crate::{Error, Result};
use gimli::{AttributeValue, DwarfSections, EndianSlice, LittleEndian, SectionId};
use object::{Object, ObjectSection, ObjectSymbol};
use std::{borrow::Cow, collections::HashMap, fs, path::Path};

pub struct Symbols {
    functions: HashMap<String, Vec<u64>>,
    symbols: HashMap<String, Vec<u64>>,
}

impl Symbols {
    pub fn load(path: &Path) -> Result<Self> {
        let data = fs::read(path)?;
        let object = object::File::parse(data.as_slice())?;
        let sections = DwarfSections::load(|id: SectionId| -> Result<Cow<'_, [u8]>> {
            match object.section_by_name(id.name()) {
                Some(section) => Ok(section.uncompressed_data()?),
                None => Ok(Cow::Borrowed(&[])),
            }
        })?;
        let dwarf = sections.borrow(|section| EndianSlice::new(section, LittleEndian));
        let mut functions: HashMap<String, Vec<u64>> = HashMap::new();
        let mut units = dwarf.units();
        while let Some(header) = units.next()? {
            let unit = dwarf.unit(header)?;
            let mut entries = unit.entries();
            while let Some(entry) = entries.next_dfs()? {
                if entry.tag() != gimli::DW_TAG_subprogram {
                    continue;
                }
                let Some(AttributeValue::Addr(address)) = entry.attr_value(gimli::DW_AT_low_pc)
                else {
                    continue;
                };
                let Some(name) = entry.attr(gimli::DW_AT_name) else {
                    continue;
                };
                let name = dwarf
                    .attr_string(&unit, name.value())?
                    .to_string_lossy()
                    .into_owned();
                functions.entry(name).or_default().push(address);
            }
        }
        let mut symbols: HashMap<String, Vec<u64>> = HashMap::new();
        for symbol in object.symbols().chain(object.dynamic_symbols()) {
            if symbol.address() == 0 {
                continue;
            }
            if let Ok(name) = symbol.name() {
                let values = symbols.entry(name.to_owned()).or_default();
                if !values.contains(&symbol.address()) {
                    values.push(symbol.address());
                }
            }
        }
        Ok(Self { functions, symbols })
    }

    pub fn function_address(&self, name: &str) -> Result<u64> {
        unique(
            &self.functions,
            name,
            Error::FunctionNotFound,
            Error::DuplicateFunction,
        )
    }

    pub fn symbol_address(&self, name: &str) -> Result<u64> {
        unique(
            &self.symbols,
            name,
            Error::SymbolNotFound,
            Error::DuplicateSymbol,
        )
    }
}

fn unique(
    map: &HashMap<String, Vec<u64>>,
    name: &str,
    missing: fn(String) -> Error,
    duplicate: fn(String) -> Error,
) -> Result<u64> {
    match map.get(name).map(Vec::as_slice) {
        None | Some([]) => Err(missing(name.to_owned())),
        Some([address]) => Ok(*address),
        Some(_) => Err(duplicate(name.to_owned())),
    }
}
