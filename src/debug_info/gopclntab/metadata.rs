//! Functions, inline instances, and line tables from Go's function table,
//! for code no DWARF describes, such as a stripped image's. They become the
//! same module-image records DWARF would produce, so nothing past the
//! provider knows where they came from.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use super::{GoFunction, GoPosition, GoTable, PcRun, Result};
use crate::model::LineEntry;
use crate::{
    AddressRange, BreakpointEntry, CodeInstanceId, CodeInstanceInfo, CodeInstanceKind,
    EntryProvenance, FunctionId, FunctionInfo, ImageAddress, LineNumber, LineSequenceId,
    SourceFileId, SourceLanguage, SourceLocation, StatementFlags, StatementRow,
};

/// A function's source positions, by the code each covers.
type Positions = Vec<(std::ops::Range<u64>, GoPosition)>;

/// The per-image tables the function table adds to.
pub struct Catalog<'a> {
    pub functions: &'a mut Vec<FunctionInfo>,
    pub code_instances: &'a mut Vec<CodeInstanceInfo>,
    pub statements: &'a mut Vec<StatementRow>,
    pub lines: &'a mut Vec<LineEntry>,
    pub next_sequence: &'a mut u32,
    /// Interns a source path.
    pub source_file: &'a mut dyn FnMut(PathBuf) -> SourceFileId,
}

/// Adds every function of the table whose code no existing function or line
/// describes. `code` returns the image's code bytes at an address.
pub fn complete<'code>(
    table: &GoTable,
    code: impl Fn(u64, usize) -> Option<&'code [u8]> + Copy,
    catalog: &mut Catalog<'_>,
) -> Result<()> {
    let described = Described::new(catalog.code_instances, catalog.lines);
    let mut builder = Builder {
        table,
        functions: Vec::new(),
        instances: Vec::new(),
        statements: Vec::new(),
        lines: Vec::new(),
        files: HashMap::new(),
        by_name: HashMap::new(),
        first_function: catalog.functions.len(),
        first_instance: catalog.code_instances.len(),
        next_sequence: *catalog.next_sequence,
        source_file: &mut *catalog.source_file,
    };
    // Every function is declared before any inlined call names one.
    let mut declared = Vec::new();
    for function in table.functions() {
        if function.entry < function.end && !described.contains(function.entry) {
            declared.push((function, builder.declare(function)?));
        }
    }
    for (function, (id, positions, inlined)) in declared {
        builder.define(function, id, &positions, &inlined, code)?;
    }
    *catalog.next_sequence = builder.next_sequence;
    catalog.functions.append(&mut builder.functions);
    catalog.code_instances.append(&mut builder.instances);
    catalog.statements.append(&mut builder.statements);
    catalog.lines.append(&mut builder.lines);
    Ok(())
}

/// The code that existing records already describe, merged and sorted.
struct Described(Vec<(u64, u64)>);

impl Described {
    fn new(instances: &[CodeInstanceInfo], lines: &[LineEntry]) -> Self {
        let mut ranges = instances
            .iter()
            .flat_map(|instance| instance.ranges.iter())
            .chain(lines.iter().map(|line| &line.range))
            .map(|range| (range.start.get(), range.end.get()))
            .collect::<Vec<_>>();
        ranges.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            match merged.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        Self(merged)
    }

    fn contains(&self, address: u64) -> bool {
        let after = self.0.partition_point(|(start, _)| *start <= address);
        after > 0 && address < self.0[after - 1].1
    }
}

struct Builder<'a, 'catalog> {
    table: &'a GoTable,
    functions: Vec<FunctionInfo>,
    instances: Vec<CodeInstanceInfo>,
    statements: Vec<StatementRow>,
    lines: Vec<LineEntry>,
    files: HashMap<Arc<str>, SourceFileId>,
    /// The function each name names, for inlined calls: the first function
    /// of that name that is not a wrapper.
    by_name: HashMap<Arc<str>, FunctionId>,
    first_function: usize,
    first_instance: usize,
    next_sequence: u32,
    source_file: &'catalog mut dyn FnMut(PathBuf) -> SourceFileId,
}

impl Builder<'_, '_> {
    fn file(&mut self, path: &Arc<str>) -> SourceFileId {
        if let Some(id) = self.files.get(path) {
            return *id;
        }
        let id = (self.source_file)(PathBuf::from(path.as_ref()));
        self.files.insert(Arc::clone(path), id);
        id
    }

    fn location(&mut self, file: &Arc<str>, line: i32) -> Option<SourceLocation> {
        let line = LineNumber::new(u64::try_from(line).ok()?)?;
        Some(SourceLocation {
            file: self.file(file),
            line,
            column: None,
        })
    }

    fn new_function(
        &mut self,
        name: Arc<str>,
        declaration: Option<SourceLocation>,
        language: SourceLanguage,
        role: crate::CodeRole,
    ) -> Result<FunctionId> {
        let id = FunctionId::new(
            u32::try_from(self.first_function + self.functions.len())
                .map_err(|_| super::malformed("too many functions"))?,
        );
        self.functions.push(FunctionInfo {
            id,
            name,
            linkage_name: None,
            declaration,
            language,
            role,
        });
        Ok(id)
    }

    fn new_instance(
        &mut self,
        function: FunctionId,
        parent: Option<CodeInstanceId>,
        kind: CodeInstanceKind,
        ranges: Vec<AddressRange<ImageAddress>>,
        breakpoint_entry: Option<BreakpointEntry>,
    ) -> Result<CodeInstanceId> {
        let id = CodeInstanceId::new(
            u32::try_from(self.first_instance + self.instances.len())
                .map_err(|_| super::malformed("too many code instances"))?,
        );
        self.instances.push(CodeInstanceInfo {
            id,
            function,
            parent,
            kind,
            ranges: ranges.into(),
            breakpoint_entry,
        });
        Ok(id)
    }

    /// The function's record, its source positions, and its inline-tree
    /// runs. It is declared where its own code begins, outside inlined code.
    fn declare(&mut self, function: &GoFunction) -> Result<(FunctionId, Positions, Vec<PcRun>)> {
        let table = self.table;
        let name = table.name(function)?;
        let positions = table.positions(function)?;
        let inlined = table.inline_runs(function)?;
        let declaration = match positions
            .iter()
            .find(|(pcs, _)| value_at(&inlined, pcs.start) < 0)
        {
            Some((_, position)) => {
                let file = Arc::clone(&position.file);
                self.location(&file, function.start_line)
            }
            None => None,
        };
        let go = function.is_go();
        let role = crate::debug_info::roles::symbol_role(&name);
        let language = if go {
            SourceLanguage::Go
        } else {
            SourceLanguage::Unknown
        };
        let id = self.new_function(Arc::clone(&name), declaration, language, role)?;
        // An ABI wrapper shares its function's name.
        if function.facts.special != Some(super::SpecialFunction::Wrapper) {
            self.by_name.entry(name).or_insert(id);
        }
        Ok((id, positions, inlined))
    }

    /// The function's code: its out-of-line instance, its lines, and the
    /// instances of the calls inlined into it.
    fn define<'code>(
        &mut self,
        function: &GoFunction,
        id: FunctionId,
        positions: &Positions,
        inlined: &[PcRun],
        code: impl Fn(u64, usize) -> Option<&'code [u8]>,
    ) -> Result<()> {
        let entry = self.table.prologue(function, code)?.end.map_or_else(
            || BreakpointEntry {
                address: ImageAddress::new(function.entry),
                provenance: EntryProvenance::RangeStart,
            },
            |end| BreakpointEntry {
                address: ImageAddress::new(end),
                provenance: EntryProvenance::AnalyzedPrologue,
            },
        );
        let physical = self.new_instance(
            id,
            None,
            CodeInstanceKind::OutOfLine,
            vec![range(function.entry, function.end)],
            Some(entry),
        )?;
        self.add_lines(positions)?;
        self.add_inlined_calls(function, inlined, physical)
    }

    /// One line-table sequence for the function: every run of a position is
    /// a statement, since the table marks none.
    fn add_lines(&mut self, positions: &Positions) -> Result<()> {
        if positions.is_empty() {
            return Ok(());
        }
        let sequence = LineSequenceId::new(self.next_sequence);
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| super::malformed("too many line sequences"))?;
        for (ordinal, (pcs, position)) in positions.iter().enumerate() {
            let Some(location) = self.location(&position.file, position.line) else {
                continue;
            };
            self.statements.push(StatementRow {
                address: ImageAddress::new(pcs.start),
                operation_index: 0,
                location: Some(location.clone()),
                discriminator: 0,
                flags: StatementFlags::empty().with_statement(true),
                isa: 0,
                sequence,
                ordinal: u32::try_from(ordinal)
                    .map_err(|_| super::malformed("too many line rows"))?,
            });
            self.lines.push(LineEntry {
                range: range(pcs.start, pcs.end),
                location,
                statement: true,
            });
        }
        Ok(())
    }

    /// An inline instance for each entry of the function's inline tree,
    /// covering the code where the entry is on the inline-call chain.
    fn add_inlined_calls(
        &mut self,
        function: &GoFunction,
        runs: &[PcRun],
        physical: CodeInstanceId,
    ) -> Result<()> {
        let table = self.table;
        // Each entry's ranges and its parent entry, by tree index.
        let mut entries = BTreeMap::<i32, (Vec<AddressRange<ImageAddress>>, i32)>::new();
        for run in runs.iter().filter(|run| run.value >= 0) {
            let mut index = run.value;
            for depth in 0.. {
                if index < 0 {
                    break;
                }
                if depth == MAX_INLINE_DEPTH {
                    return Err(super::malformed("the inline tree is too deep"));
                }
                let parent = if let Some((_, parent)) = entries.get(&index) {
                    *parent
                } else {
                    value_at(runs, table.inlined_call(function, index)?.parent_pc)
                };
                if parent == index {
                    return Err(super::malformed("an inlined call is its own caller"));
                }
                let (ranges, _) = entries.entry(index).or_insert_with(|| (Vec::new(), parent));
                match ranges.last_mut() {
                    Some(last) if last.end.get() == run.start => {
                        last.end = ImageAddress::new(run.end);
                    }
                    _ => ranges.push(range(run.start, run.end)),
                }
                index = parent;
            }
        }
        let mut instances = BTreeMap::<i32, CodeInstanceId>::new();
        // Parents first, so each instance names its parent's.
        let mut pending = entries.keys().copied().collect::<Vec<_>>();
        while !pending.is_empty() {
            let before = pending.len();
            let mut deferred = Vec::new();
            for index in pending {
                let (ranges, parent) = &entries[&index];
                let parent_instance = if *parent < 0 {
                    physical
                } else if let Some(instance) = instances.get(parent) {
                    *instance
                } else {
                    deferred.push(index);
                    continue;
                };
                let call = table.inlined_call(function, index)?;
                let name = table.inlined_name(&call)?;
                let call_site = table
                    .position(function, call.parent_pc)?
                    .and_then(|position| self.location(&position.file, position.line));
                let callee = match self.by_name.get(&name) {
                    Some(id) => *id,
                    None => self.inlined_function(function, &call, name, ranges)?,
                };
                let instance = self.new_instance(
                    callee,
                    Some(parent_instance),
                    CodeInstanceKind::Inline { call_site },
                    ranges.clone(),
                    ranges.first().map(|first| BreakpointEntry {
                        address: first.start,
                        provenance: EntryProvenance::RangeStart,
                    }),
                )?;
                instances.insert(index, instance);
            }
            if deferred.len() == before {
                return Err(super::malformed("the inline tree's calls form a cycle"));
            }
            pending = deferred;
        }
        Ok(())
    }
}

impl Builder<'_, '_> {
    /// A function for an inlined call whose callee has no code of its own,
    /// declared where its first inlined code is.
    fn inlined_function(
        &mut self,
        function: &GoFunction,
        call: &super::InlinedCall,
        name: Arc<str>,
        ranges: &[AddressRange<ImageAddress>],
    ) -> Result<FunctionId> {
        let declaration = match ranges.first() {
            Some(first) => self
                .table
                .position(function, first.start.get())?
                .and_then(|position| self.location(&position.file, call.start_line)),
            None => None,
        };
        let role = crate::debug_info::roles::symbol_role(&name);
        let id = self.new_function(Arc::clone(&name), declaration, SourceLanguage::Go, role)?;
        self.by_name.insert(name, id);
        Ok(id)
    }
}

/// Bounds the inline-call chain at one address, which only a corrupt tree
/// makes longer than the compiler's inlining depth.
const MAX_INLINE_DEPTH: usize = 256;

/// The value of the run containing an address, or -1 outside every run.
fn value_at(runs: &[PcRun], address: u64) -> i32 {
    let after = runs.partition_point(|run| run.start <= address);
    runs[..after]
        .last()
        .filter(|run| address < run.end)
        .map_or(-1, |run| run.value)
}

const fn range(start: u64, end: u64) -> AddressRange<ImageAddress> {
    AddressRange {
        start: ImageAddress::new(start),
        end: ImageAddress::new(end),
    }
}
