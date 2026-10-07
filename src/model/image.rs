//! A module image's static metadata and the indexes that answer lookups
//! in it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{Error, Result};

mod locations;

pub use locations::PackageInfo;

use super::{
    AddressRange, BreakpointEntry, CodeInstanceId, CodeInstanceInfo, CodeInstanceKind, CodeRole,
    EntryProvenance, FunctionId, FunctionInfo, GlobalVariableId, GlobalVariableInfo, ImageAddress,
    ImageAddressDescription, ImageLocation, InlineChain, InlineFrameLookup, LineEntry, LineNumber,
    ModuleImageId, SectionId, SectionInfo, SectionLocation, SourceFile, SourceFileId,
    SourceLanguage, SourceLocation, StatementRow, SymbolExtentProvenance, SymbolId, SymbolInfo,
    SymbolKind, SymbolLocation, SymbolTableSources, TargetDescription, TypeInfo, TypeNode,
    TypeReference,
};

#[derive(Default)]
pub struct ModuleMetadata {
    pub functions: Vec<FunctionInfo>,
    pub code_instances: Vec<CodeInstanceInfo>,
    pub symbols: Vec<SymbolInfo>,
    pub symbol_sources: SymbolTableSources,
    pub globals: Vec<GlobalVariableInfo>,
    pub types: Arc<[TypeNode]>,
    pub source_files: Vec<SourceFile>,
    pub statements: Vec<StatementRow>,
    pub lines: Vec<LineEntry>,
    pub sections: Vec<SectionInfo>,
    /// Rust trait objects' vtables, by address, with the concrete type each
    /// is for.
    pub vtables: Vec<(ImageAddress, TypeReference)>,
    /// Whether each thread gets its own copy of a block of the module's
    /// storage.
    pub thread_local_storage: bool,
    /// Integer constants the debug information declares by name, such as a
    /// Go package's `const`s.
    pub constants: BTreeMap<Arc<str>, crate::IntegerValue>,
    /// The distinct compilers and versions that produced the debug
    /// information, as each unit names its producer.
    pub producers: Vec<Arc<str>>,
    /// The packages whose units the image has.
    pub packages: Vec<PackageInfo>,
    /// Where each thread's copy of each of the image's thread-local
    /// variables is, by name, or why that is unknown.
    pub thread_locals: BTreeMap<Arc<str>, std::result::Result<ThreadLocal, Arc<str>>>,
}

/// Where a thread's copy of a thread-local variable is, relative to the
/// thread's thread pointer, as the image's own code finds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadLocal {
    /// This far from it, fixed when the program was linked, as an
    /// executable's own thread-locals are.
    Offset(i64),
    /// As far from it as the word at this address says, which the loader
    /// writes as it loads the image, as a library's code reads its
    /// thread-locals.
    Slot(ImageAddress),
}

#[derive(Debug)]
struct RangeIndexEntry<T> {
    start: u64,
    end: u64,
    prefix_max_end: u64,
    value: T,
}

#[derive(Debug)]
struct RangeIndex<T> {
    entries: Arc<[RangeIndexEntry<T>]>,
}

impl<T: Copy + Ord> RangeIndex<T> {
    fn new(entries: impl IntoIterator<Item = (AddressRange<ImageAddress>, T)>) -> Self {
        let mut entries = entries
            .into_iter()
            .map(|(range, value)| RangeIndexEntry {
                start: range.start.get(),
                end: range.end.get(),
                prefix_max_end: 0,
                value,
            })
            .collect::<Vec<_>>();
        entries.sort_unstable_by_key(|entry| (entry.start, entry.end, entry.value));

        let mut prefix_max_end = 0;
        for entry in &mut entries {
            prefix_max_end = prefix_max_end.max(entry.end);
            entry.prefix_max_end = prefix_max_end;
        }

        Self {
            entries: entries.into(),
        }
    }

    fn containing(&self, address: ImageAddress) -> impl Iterator<Item = T> + '_ {
        let address = address.get();
        let mut index = self.entries.partition_point(|entry| entry.start <= address);

        std::iter::from_fn(move || {
            while index > 0 {
                index -= 1;
                let entry = &self.entries[index];
                if entry.prefix_max_end <= address {
                    return None;
                }
                if address < entry.end {
                    return Some(entry.value);
                }
            }

            None
        })
    }
}

/// Groups values by key, each group sorted and without duplicates.
fn grouped_index<K: Ord, V: Ord>(
    entries: impl IntoIterator<Item = (K, V)>,
) -> BTreeMap<K, Arc<[V]>> {
    let mut grouped = BTreeMap::<K, BTreeSet<V>>::new();
    for (key, value) in entries {
        grouped.entry(key).or_default().insert(value);
    }
    grouped
        .into_iter()
        .map(|(key, values)| (key, values.into_iter().collect()))
        .collect()
}

/// Every selector naming a global: its name, qualified name, and linkage
/// name, and its qualified name after its declaring file's path or name.
fn global_selectors(metadata: &ModuleMetadata) -> Vec<(Arc<str>, GlobalVariableId)> {
    let mut selectors = Vec::new();
    for global in &metadata.globals {
        selectors.push((Arc::clone(&global.name), global.id));
        selectors.push((Arc::clone(&global.qualified_name), global.id));
        if let Some(linkage_name) = &global.linkage_name {
            selectors.push((Arc::clone(linkage_name), global.id));
        }
        if let Some(declaration) = &global.declaration
            && let Some(source) = metadata.source_files.get(declaration.file.index())
        {
            let path = source.path.to_string_lossy();
            selectors.push((
                format!("{path}::{}", global.qualified_name).into(),
                global.id,
            ));
            if let Some(file_name) = source.path.file_name() {
                let file_name = file_name.to_string_lossy();
                selectors.push((
                    format!("{file_name}::{}", global.qualified_name).into(),
                    global.id,
                ));
            }
        }
    }
    selectors
}

/// The entries of each code instance: every distinct `prologue_end` address
/// within an out-of-line instance, or otherwise its own breakpoint entry.
fn recommended_entries(
    metadata: &ModuleMetadata,
    code_range_index: &RangeIndex<CodeInstanceId>,
) -> BTreeMap<CodeInstanceId, Arc<[BreakpointEntry]>> {
    let mut prologue_ends = BTreeMap::<CodeInstanceId, Vec<BreakpointEntry>>::new();
    for row in metadata
        .statements
        .iter()
        .filter(|row| row.flags.prologue_end())
    {
        for id in code_range_index.containing(row.address) {
            if !matches!(
                metadata.code_instances[id.index()].kind,
                CodeInstanceKind::OutOfLine
            ) {
                continue;
            }
            let entries = prologue_ends.entry(id).or_default();
            if !entries.iter().any(|entry| entry.address == row.address) {
                entries.push(BreakpointEntry {
                    address: row.address,
                    provenance: EntryProvenance::Statement,
                });
            }
        }
    }
    metadata
        .code_instances
        .iter()
        .filter_map(|instance| {
            let entries = match prologue_ends.remove(&instance.id) {
                Some(entries) => entries,
                None => vec![instance.breakpoint_entry?],
            };
            Some((instance.id, entries.into()))
        })
        .collect()
}

/// Ends each line entry where a function symbol begins inside it. A line
/// program describes every function it covers from the function's first
/// instruction, but its last row before code it does not describe, such as
/// hand-written assembly placed after a compiled function, runs on to the
/// next row: that code has no source line.
fn clip_lines_at_functions(lines: &mut [LineEntry], symbols: &[SymbolInfo]) {
    let mut starts = symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Function && symbol.extent.is_some())
        .map(|symbol| symbol.address)
        .collect::<Vec<_>>();
    starts.sort_unstable();
    starts.dedup();
    for line in lines {
        let after = starts.partition_point(|start| *start <= line.range.start);
        if let Some(&start) = starts.get(after)
            && start < line.range.end
        {
            line.range.end = start;
        }
    }
}

fn validate_dense_ids(metadata: &ModuleMetadata) {
    for (index, function) in metadata.functions.iter().enumerate() {
        assert_eq!(
            function.id.index(),
            index,
            "function IDs are dense and ordered"
        );
    }
    for (index, instance) in metadata.code_instances.iter().enumerate() {
        assert_eq!(
            instance.id.index(),
            index,
            "code instance IDs are dense and ordered"
        );
    }
    for (index, source_file) in metadata.source_files.iter().enumerate() {
        assert_eq!(
            source_file.id.index(),
            index,
            "source file IDs are dense and ordered"
        );
    }
    for (index, symbol) in metadata.symbols.iter().enumerate() {
        assert_eq!(symbol.id.index(), index, "symbol IDs are dense and ordered");
        if let Some(extent) = symbol.extent {
            assert!(
                matches!(
                    symbol.kind,
                    SymbolKind::Function | SymbolKind::IndirectFunction
                ) && extent.range.start == symbol.address
                    && extent.range.start < extent.range.end,
                "symbol extents are non-empty code ranges beginning at the symbol"
            );
        }
        if let Some(storage) = symbol.storage {
            assert!(
                symbol.kind == SymbolKind::Data
                    && symbol.extent.is_none()
                    && storage.start == symbol.address
                    && storage.start <= storage.end,
                "symbol storage is a data range beginning at the symbol"
            );
        }
    }
    for (index, section) in metadata.sections.iter().enumerate() {
        assert_eq!(
            section.id.index(),
            index,
            "section IDs are dense and ordered"
        );
        assert!(
            section.range.start < section.range.end,
            "sections are non-empty"
        );
    }
    for (index, global) in metadata.globals.iter().enumerate() {
        assert_eq!(global.id.index(), index, "global IDs are dense and ordered");
    }
    for (index, node) in metadata.types.iter().enumerate() {
        assert_eq!(
            usize::try_from(node.reference().id.get()).expect("type ID fits usize"),
            index,
            "type IDs are dense and ordered"
        );
    }
}

/// Immutable, normalized debug metadata for one ELF module image.
#[derive(Debug)]
pub struct ModuleImage {
    id: ModuleImageId,
    path: Arc<PathBuf>,
    target: TargetDescription,
    address_range: AddressRange<ImageAddress>,
    functions: Arc<[FunctionInfo]>,
    code_instances: Arc<[CodeInstanceInfo]>,
    symbols: Arc<[SymbolInfo]>,
    symbol_sources: SymbolTableSources,
    sections: Arc<[SectionInfo]>,
    thread_local_storage: bool,
    globals: Arc<[GlobalVariableInfo]>,
    types: Arc<[TypeNode]>,
    source_files: Arc<[SourceFile]>,
    statements: Arc<[StatementRow]>,
    lines: Arc<[LineEntry]>,
    functions_by_name: BTreeMap<Arc<str>, Arc<[FunctionId]>>,
    /// Functions by their names within the packages defining them.
    function_names: locations::FunctionNames,
    symbols_by_name: BTreeMap<Arc<str>, Arc<[SymbolId]>>,
    globals_by_selector: BTreeMap<Arc<str>, Arc<[GlobalVariableId]>>,
    instances_by_function: BTreeMap<FunctionId, Arc<[CodeInstanceId]>>,
    statements_by_source_line: BTreeMap<(SourceFileId, LineNumber), Arc<[ImageAddress]>>,
    control_boundaries_by_address: BTreeMap<ImageAddress, Arc<[u32]>>,
    recommended_entries_by_instance: BTreeMap<CodeInstanceId, Arc<[BreakpointEntry]>>,
    code_range_index: RangeIndex<CodeInstanceId>,
    line_range_index: RangeIndex<u32>,
    symbol_range_index: RangeIndex<SymbolId>,
    storage_range_index: RangeIndex<SymbolId>,
    /// Unsized data symbols, each indexed by its one-byte address.
    unsized_data_index: RangeIndex<SymbolId>,
    section_range_index: RangeIndex<SectionId>,
    /// Known instruction starts in address order, one per address.
    instruction_starts: Arc<[(ImageAddress, crate::BoundaryEvidence)]>,
    type_index: crate::type_identity::TypeIndex,
    /// Rust trait objects' vtables, with the concrete type each is for.
    vtables: std::collections::BTreeMap<ImageAddress, TypeReference>,
    constants: BTreeMap<Arc<str>, crate::IntegerValue>,
    producers: Arc<[Arc<str>]>,
    thread_locals: BTreeMap<Arc<str>, std::result::Result<ThreadLocal, Arc<str>>>,
    /// The index in `types` of the first type each Go runtime type
    /// descriptor offset names.
    go_runtime_types: std::collections::BTreeMap<u64, usize>,
    /// The views the image carries for its own types, in its
    /// `.debug_uscope_views` section.
    views: Arc<crate::view::ViewSet>,
}

/// The first type, in identifier order, that each Go runtime type
/// descriptor offset names: a named type and its typedef may both.
fn go_runtime_types(types: &[TypeNode]) -> std::collections::BTreeMap<u64, usize> {
    let mut offsets = std::collections::BTreeMap::new();
    for (index, node) in types.iter().enumerate() {
        if let TypeNode::Resolved(info) = node
            && let Some(offset) = info
                .identity
                .as_ref()
                .and_then(|identity| identity.go)
                .and_then(|go| go.runtime_type)
        {
            offsets.entry(offset).or_insert(index);
        }
    }
    offsets
}

impl ModuleImage {
    #[expect(clippy::too_many_lines, reason = "one constructor builds every index")]
    pub(crate) fn new(
        path: PathBuf,
        target: TargetDescription,
        address_range: AddressRange<ImageAddress>,
        mut metadata: ModuleMetadata,
    ) -> Self {
        validate_dense_ids(&metadata);
        clip_lines_at_functions(&mut metadata.lines, &metadata.symbols);
        let code_range_index =
            RangeIndex::new(metadata.code_instances.iter().flat_map(|instance| {
                instance
                    .ranges
                    .iter()
                    .copied()
                    .map(|range| (range, instance.id))
            }));
        let line_range_index =
            RangeIndex::new(metadata.lines.iter().enumerate().map(|(index, line)| {
                (
                    line.range,
                    u32::try_from(index).expect("line entry count fits u32"),
                )
            }));
        let symbol_range_index = RangeIndex::new(
            metadata
                .symbols
                .iter()
                .filter_map(|symbol| Some((symbol.extent?.range, symbol.id))),
        );
        let storage_range_index = RangeIndex::new(
            metadata
                .symbols
                .iter()
                .filter_map(|symbol| Some((symbol.storage?, symbol.id))),
        );
        let unsized_data_index = RangeIndex::new(metadata.symbols.iter().filter_map(|symbol| {
            let storage = symbol.storage?;
            (storage.start == storage.end).then_some((
                AddressRange {
                    start: storage.start,
                    end: ImageAddress::new(storage.start.get().checked_add(1)?),
                },
                symbol.id,
            ))
        }));
        let instruction_starts = instruction_starts(&metadata);
        let section_range_index = RangeIndex::new(
            metadata
                .sections
                .iter()
                .map(|section| (section.range, section.id)),
        );

        let type_index = crate::type_identity::TypeIndex::build(
            metadata
                .types
                .first()
                .map(TypeNode::reference)
                .map(|reference| reference.image),
            metadata.types.len(),
            |index| match &metadata.types[index] {
                TypeNode::Resolved(info) => Some(info),
                TypeNode::Malformed { .. } => None,
            },
        );

        Self {
            functions_by_name: grouped_index(
                metadata
                    .functions
                    .iter()
                    .map(|function| (Arc::clone(&function.name), function.id)),
            ),
            function_names: locations::FunctionNames::new(&metadata.functions, &metadata.packages),
            symbols_by_name: grouped_index(
                metadata
                    .symbols
                    .iter()
                    .map(|symbol| (Arc::clone(&symbol.name), symbol.id)),
            ),
            globals_by_selector: grouped_index(global_selectors(&metadata)),
            instances_by_function: grouped_index(
                metadata
                    .code_instances
                    .iter()
                    .map(|instance| (instance.function, instance.id)),
            ),
            statements_by_source_line: grouped_index(metadata.statements.iter().filter_map(
                |row| {
                    let location = row.location.as_ref()?;
                    row.flags
                        .is_statement()
                        .then_some(((location.file, location.line), row.address))
                },
            )),
            control_boundaries_by_address: grouped_index(
                metadata
                    .statements
                    .iter()
                    .enumerate()
                    .filter(|(_, row)| row.flags.prologue_end() || row.flags.epilogue_begin())
                    .map(|(index, row)| {
                        let index = u32::try_from(index).expect("line-program row count fits u32");
                        (row.address, index)
                    }),
            ),
            recommended_entries_by_instance: recommended_entries(&metadata, &code_range_index),
            id: ModuleImageId::new(0),
            path: Arc::new(path),
            target,
            address_range,
            functions: metadata.functions.into(),
            code_instances: metadata.code_instances.into(),
            symbols: metadata.symbols.into(),
            symbol_sources: metadata.symbol_sources,
            sections: metadata.sections.into(),
            thread_local_storage: metadata.thread_local_storage,
            globals: metadata.globals.into(),
            types: Arc::clone(&metadata.types),
            source_files: metadata.source_files.into(),
            statements: metadata.statements.into(),
            lines: metadata.lines.into(),
            code_range_index,
            line_range_index,
            symbol_range_index,
            storage_range_index,
            unsized_data_index,
            section_range_index,
            instruction_starts,
            type_index,
            vtables: metadata.vtables.iter().copied().collect(),
            constants: std::mem::take(&mut metadata.constants),
            producers: std::mem::take(&mut metadata.producers).into(),
            thread_locals: std::mem::take(&mut metadata.thread_locals),
            go_runtime_types: go_runtime_types(&metadata.types),
            views: crate::view::ViewSet::empty(),
        }
    }

    pub(crate) fn with_id(mut self, id: ModuleImageId) -> Self {
        assert!(
            self.types.iter().all(|node| node.reference().image == id),
            "every type node is owned by its module image"
        );
        self.id = id;
        self
    }

    /// Gives the image the views it carries for its own types.
    pub(crate) fn with_views(mut self, views: Arc<crate::view::ViewSet>) -> Self {
        self.views = views;
        self
    }

    /// The views the image carries for its own types.
    #[must_use]
    pub(crate) const fn views(&self) -> &Arc<crate::view::ViewSet> {
        &self.views
    }

    /// A type's identity as one string, which every type the same as it
    /// shares.
    #[must_use]
    pub(crate) fn type_key(&self, reference: TypeReference) -> Option<&Arc<str>> {
        self.type_index.key(reference)
    }

    /// What kept parts of the views the image carries out.
    #[must_use]
    pub fn view_errors(&self) -> &[crate::ViewFileError] {
        self.views.errors()
    }

    /// Returns this image's session-scoped identifier.
    #[must_use]
    pub const fn id(&self) -> ModuleImageId {
        self.id
    }

    /// Returns the executable path used to load this image.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn path_arc(&self) -> Arc<PathBuf> {
        Arc::clone(&self.path)
    }

    /// Returns the properties of this image's target.
    #[must_use]
    pub const fn target(&self) -> TargetDescription {
        self.target
    }

    /// Returns whether an image address lies in this module's loadable range.
    #[must_use]
    pub fn contains_address(&self, address: ImageAddress) -> bool {
        self.address_range.contains(address)
    }

    /// Returns the image addresses this module's loadable segments span.
    #[must_use]
    pub const fn address_range(&self) -> AddressRange<ImageAddress> {
        self.address_range
    }

    /// Returns all functions described by this image.
    #[must_use]
    pub fn functions(&self) -> &[FunctionInfo] {
        &self.functions
    }

    /// Looks up a source-level function by identifier.
    #[must_use]
    pub fn function(&self, id: FunctionId) -> Option<&FunctionInfo> {
        self.functions.get(id.index())
    }

    /// Returns all concrete code instances described by this image.
    #[must_use]
    pub fn code_instances(&self) -> &[CodeInstanceInfo] {
        &self.code_instances
    }

    /// Returns all linker symbols described by this image, ordered by
    /// address and then name.
    #[must_use]
    pub fn symbols(&self) -> &[SymbolInfo] {
        &self.symbols
    }

    /// Looks up a linker symbol by identifier.
    #[must_use]
    pub fn symbol(&self, id: SymbolId) -> Option<&SymbolInfo> {
        self.symbols.get(id.index())
    }

    /// Returns the image's allocated sections, ordered by address.
    #[must_use]
    pub fn sections(&self) -> &[SectionInfo] {
        &self.sections
    }

    /// Looks up an allocated section by identifier.
    #[must_use]
    pub fn section(&self, id: SectionId) -> Option<&SectionInfo> {
        self.sections.get(id.index())
    }

    /// Whether each thread gets its own copy of a block of the module's
    /// storage, such as an ELF `PT_TLS` segment describes.
    #[must_use]
    pub const fn has_thread_local_storage(&self) -> bool {
        self.thread_local_storage
    }

    /// Finds the allocated section containing an image address. Should
    /// malformed sections overlap, the innermost one wins.
    fn section_containing(&self, address: ImageAddress) -> Option<&SectionInfo> {
        self.section_range_index
            .containing(address)
            .filter_map(|id| self.section(id))
            .min_by_key(|section| {
                (
                    std::cmp::Reverse(section.range.start),
                    section.range.end,
                    section.id,
                )
            })
    }

    /// Returns which symbol tables this image provided.
    #[must_use]
    pub const fn symbol_sources(&self) -> &SymbolTableSources {
        &self.symbol_sources
    }

    /// Finds the code symbol whose extent contains an image address.
    ///
    /// Declared extents take precedence over inferred ones. Among the
    /// remaining candidates the innermost extent wins, then a function over an
    /// indirect-function resolver, then global over weak over local binding,
    /// then an exported symbol, then the name with the fewest leading
    /// underscores, then the bytewise-smallest name. An address that no extent
    /// contains has no symbol; the nearest preceding symbol is never guessed.
    #[must_use]
    pub fn symbolize(&self, address: ImageAddress) -> Option<SymbolLocation> {
        let symbol = self
            .symbol_range_index
            .containing(address)
            .filter_map(|id| self.symbol(id))
            .min_by_key(|symbol| symbol_preference(symbol))?;
        let extent = symbol.extent.expect("indexed symbols have extents");

        Some(SymbolLocation {
            symbol: symbol.id,
            name: Arc::clone(&symbol.name),
            kind: symbol.kind,
            offset: address.get() - symbol.address.get(),
            provenance: extent.provenance,
        })
    }

    /// Finds the data symbol naming an image address: the one whose declared
    /// storage contains it, choosing among overlapping storage as
    /// [`Self::symbolize`] chooses among code extents, or otherwise an
    /// unsized data symbol at exactly that address. An address inside no
    /// declared storage is never attributed to the nearest preceding object.
    fn symbolize_data(&self, address: ImageAddress) -> Option<SymbolLocation> {
        let symbol = self
            .storage_range_index
            .containing(address)
            .filter_map(|id| self.symbol(id))
            .min_by_key(|symbol| storage_preference(symbol))
            .or_else(|| {
                self.unsized_data_index
                    .containing(address)
                    .filter_map(|id| self.symbol(id))
                    .min_by_key(|symbol| storage_preference(symbol))
            })?;
        let storage = symbol.storage.expect("indexed symbols have storage");

        Some(SymbolLocation {
            symbol: symbol.id,
            name: Arc::clone(&symbol.name),
            kind: symbol.kind,
            offset: address.get() - symbol.address.get(),
            provenance: if storage.start < storage.end {
                SymbolExtentProvenance::Declared
            } else {
                SymbolExtentProvenance::Inferred
            },
        })
    }

    /// Returns the addresses within `range` known to begin an instruction:
    /// the start of every range of an out-of-line function instance, of
    /// every code symbol with an extent, and of every executable section.
    /// Line table rows are excluded because some producers emit rows inside
    /// instructions, such as Go after a `LOCK` prefix.
    pub fn instruction_starts(
        &self,
        range: AddressRange<ImageAddress>,
    ) -> impl Iterator<Item = (ImageAddress, crate::BoundaryEvidence)> + '_ {
        let first = self
            .instruction_starts
            .partition_point(|(address, _)| *address < range.start);
        self.instruction_starts[first..]
            .iter()
            .take_while(move |(address, _)| *address < range.end)
            .copied()
    }

    /// Returns the source line containing an image address, when the line
    /// table describes it.
    #[must_use]
    pub fn source_location(&self, address: ImageAddress) -> Option<SourceLocation> {
        self.line_entry_containing(address)
            .map(|entry| entry.location.clone())
    }

    /// Describes an image address by its section and by the code symbol, or
    /// otherwise the data symbol, containing it.
    #[must_use]
    pub fn describe(&self, address: ImageAddress) -> ImageAddressDescription {
        ImageAddressDescription {
            address,
            section: self
                .section_containing(address)
                .map(|section| SectionLocation {
                    section: section.id,
                    name: Arc::clone(&section.name),
                    offset: address.get() - section.range.start.get(),
                    executable: section.executable,
                }),
            symbol: self
                .symbolize(address)
                .or_else(|| self.symbolize_data(address)),
        }
    }

    /// Where each thread's copy of the named thread-local variable is, or
    /// why that is unknown; `None` when the image defines none by the name.
    #[must_use]
    pub fn thread_local(&self, name: &str) -> Option<std::result::Result<ThreadLocal, Arc<str>>> {
        self.thread_locals.get(name).cloned()
    }

    /// Returns every global catalog entry in deterministic source order.
    #[must_use]
    pub fn globals(&self) -> &[GlobalVariableInfo] {
        &self.globals
    }

    /// Looks up a global catalog entry by identifier.
    #[must_use]
    pub fn global(&self, id: GlobalVariableId) -> Option<&GlobalVariableInfo> {
        self.globals.get(id.index())
    }

    /// Returns the reachable, normalized type graph in stable identifier order.
    #[must_use]
    pub fn types(&self) -> &[TypeNode] {
        &self.types
    }

    /// Resolves a reference owned by this image to its finalized graph node.
    #[must_use]
    pub fn type_node(&self, reference: TypeReference) -> Option<&TypeNode> {
        if reference.image != self.id {
            return None;
        }
        self.types
            .get(reference.id.index())
            .filter(|node| node.reference() == reference)
    }

    /// Resolves a reference to normalized metadata when the node is not malformed.
    #[must_use]
    pub fn type_info(&self, reference: TypeReference) -> Option<&TypeInfo> {
        match self.type_node(reference)? {
            TypeNode::Resolved(info) => Some(info),
            TypeNode::Malformed { .. } => None,
        }
    }

    /// The types with exactly this language, path, and base, whatever their
    /// arguments, in identifier order: every instance of a template.
    #[must_use]
    pub fn type_instances(
        &self,
        language: SourceLanguage,
        path: &[&str],
        base: &str,
    ) -> Vec<TypeReference> {
        self.type_index
            .instances(language, path, base, &self.types.as_ref())
    }

    /// The value of the integer constant the debug information declares as
    /// `name`, such as `runtime._Grunning`.
    #[must_use]
    pub fn constant(&self, name: &str) -> Option<crate::IntegerValue> {
        self.constants.get(name).copied()
    }

    /// The distinct producers of the image's debug information, such as
    /// `Go cmd/compile go1.27.1; regabi`.
    #[must_use]
    pub fn producers(&self) -> &[Arc<str>] {
        &self.producers
    }

    /// The concrete type a Rust trait object's vtable at `address` is for.
    #[must_use]
    pub fn trait_object_type(&self, address: ImageAddress) -> Option<TypeReference> {
        self.vtables.get(&address).copied()
    }

    /// The C++ class whose vtable group, `vtable for X`, holds `address`:
    /// the class's name, and where the group begins.
    #[must_use]
    pub fn vtable_class(&self, address: ImageAddress) -> Option<(String, ImageAddress)> {
        let location = self.symbolize_data(address)?;
        let symbol = self.symbol(location.symbol)?;
        let name = crate::demangle::demangle(&symbol.name)?;
        let class = name
            .strip_prefix("vtable for ")
            .or_else(|| name.strip_prefix("{vtable(")?.strip_suffix(")}"))?;
        Some((class.to_owned(), symbol.address))
    }

    /// The type Go's runtime describes at `offset` from `runtime.types`, as
    /// its `DW_AT_go_runtime_type` says: the first in identifier order when
    /// several, such as a named type and its typedef, say so.
    #[must_use]
    pub fn go_runtime_type(&self, offset: u64) -> Option<TypeReference> {
        let index = *self.go_runtime_types.get(&offset)?;
        match &self.types[index] {
            TypeNode::Resolved(info) => Some(info.reference),
            TypeNode::Malformed { .. } => None,
        }
    }

    /// The types whose identity has this base, whatever their language,
    /// path, and arguments, in identifier order.
    #[must_use]
    pub fn types_with_base(&self, base: &str) -> Vec<TypeReference> {
        self.type_index.with_base(base)
    }

    /// The types a name could mean, in identifier order: those named
    /// exactly so, and those whose identity it spells. The name may omit
    /// outer path segments and trailing arguments, as in `vector<int>` for
    /// `std::vector<int, std::allocator<int> >`.
    #[must_use]
    pub fn types_named(&self, name: &str) -> Vec<TypeReference> {
        self.type_index.named(name, false, &self.types.as_ref())
    }

    /// Whether two of this image's types have the same identity, as one
    /// type defined in several units does.
    #[must_use]
    pub fn same_type(&self, left: TypeReference, right: TypeReference) -> bool {
        self.type_index.same_type(left, right)
    }

    /// Resolves a basename, canonical qualification, source qualification, or
    /// linkage identity to exactly one catalog entry.
    pub fn global_named(&self, selector: &str) -> Result<&GlobalVariableInfo> {
        let matches = self
            .globals_by_selector
            .get(selector)
            .ok_or_else(|| Error::VariableNotFound(selector.to_owned()))?;
        let [id] = matches.as_ref() else {
            return Err(Error::AmbiguousGlobalVariable {
                selector: selector.to_owned(),
                candidates: matches
                    .iter()
                    .filter_map(|id| self.global(*id))
                    .map(|global| crate::GlobalVariableCandidate {
                        id: global.id,
                        qualified_name: Arc::clone(&global.qualified_name),
                        declaration_path: global
                            .declaration
                            .as_ref()
                            .and_then(|declaration| self.source_file(declaration.file))
                            .map(|source| Arc::clone(&source.path)),
                        declaration: global.declaration.clone(),
                    })
                    .collect(),
            });
        };
        Ok(self
            .global(*id)
            .expect("global index references a catalog entry"))
    }

    /// Returns all source files referenced by this image.
    #[must_use]
    pub fn source_files(&self) -> &[SourceFile] {
        &self.source_files
    }

    /// Finds one source file using an absolute path or trailing path components.
    pub fn source_file_matching(&self, path: &Path) -> Result<&SourceFile> {
        let matches = self
            .source_files
            .iter()
            .filter(|source| path_matches(source.path.as_path(), path))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [source] => Ok(source),
            [] => Err(Error::SourceFileNotFound(path.to_path_buf())),
            _ => Err(Error::AmbiguousSourceFile {
                path: path.to_path_buf(),
                matches: matches
                    .iter()
                    .map(|source| source.path.as_ref().clone())
                    .collect(),
            }),
        }
    }

    /// Returns every ordered source line-program row in this image.
    #[must_use]
    pub fn statement_rows(&self) -> &[StatementRow] {
        &self.statements
    }

    /// Returns exact line-program control boundaries at an image address.
    ///
    /// Equal-address rows remain distinct and retain their sequence and
    /// ordinal. This query does not infer an epilogue region after an
    /// `epilogue_begin` marker.
    pub fn control_boundaries_at(
        &self,
        address: ImageAddress,
    ) -> impl Iterator<Item = &StatementRow> {
        self.control_boundaries_by_address
            .get(&address)
            .into_iter()
            .flat_map(|rows| rows.iter())
            .filter_map(|row| self.statements.get(*row as usize))
    }

    /// Returns where a function breakpoint enters one code instance: every
    /// `prologue_end` address of an out-of-line instance, or otherwise the
    /// instance's own breakpoint entry.
    pub fn recommended_entries_for_instance(
        &self,
        instance: CodeInstanceId,
    ) -> impl Iterator<Item = BreakpointEntry> + '_ {
        self.recommended_entries_by_instance
            .get(&instance)
            .into_iter()
            .flat_map(|entries| entries.iter())
            .copied()
    }

    pub(crate) fn line_entries(&self) -> &[LineEntry] {
        &self.lines
    }

    pub(crate) fn line_entry_containing(&self, address: ImageAddress) -> Option<&LineEntry> {
        self.line_range_index
            .containing(address)
            .min()
            .and_then(|index| {
                self.lines
                    .get(usize::try_from(index).expect("u32 fits usize"))
            })
    }

    /// Looks up a concrete code instance by identifier.
    #[must_use]
    pub fn code_instance(&self, id: CodeInstanceId) -> Option<&CodeInstanceInfo> {
        self.code_instances.get(id.index())
    }

    /// Returns the concrete instances of one source-level function.
    pub fn instances_for_function(
        &self,
        function: FunctionId,
    ) -> impl Iterator<Item = &CodeInstanceInfo> {
        self.instances_by_function
            .get(&function)
            .into_iter()
            .flat_map(|instances| instances.iter())
            .filter_map(|instance| self.code_instance(*instance))
    }

    /// Returns image addresses associated with one source line.
    pub fn statement_addresses(
        &self,
        file: SourceFileId,
        line: LineNumber,
    ) -> impl Iterator<Item = ImageAddress> + '_ {
        self.statements_by_source_line
            .get(&(file, line))
            .into_iter()
            .flat_map(|addresses| addresses.iter())
            .copied()
    }

    /// Returns the lines of one source file within `lines` that have
    /// statement addresses: the lines a source breakpoint stops at as
    /// requested.
    pub fn breakpoint_lines(
        &self,
        file: SourceFileId,
        lines: std::ops::RangeInclusive<LineNumber>,
    ) -> impl Iterator<Item = LineNumber> + '_ {
        self.statements_by_source_line
            .range((file, *lines.start())..=(file, *lines.end()))
            .map(|((_, line), _)| *line)
    }

    /// Finds the line a source breakpoint requested at `line` stops at, as
    /// gdb does: the line itself when it has statements, otherwise the next
    /// line that does, provided a function whose statements begin at or
    /// before the request contains it. A line between functions never moves
    /// into the next one, and a line of a Go file never moves at all.
    #[must_use]
    pub fn breakpoint_line(&self, file: SourceFileId, line: LineNumber) -> Option<LineNumber> {
        let ((_, next), addresses) = self
            .statements_by_source_line
            .range((file, line)..)
            .next()
            .filter(|((next_file, _), _)| *next_file == file)?;
        if *next == line {
            return Some(line);
        }
        if self.keeps_line_breakpoints(file) {
            return None;
        }
        let encloses_request = |instance: &CodeInstanceInfo| {
            self.statements.iter().any(|row| {
                row.flags.is_statement()
                    && instance.contains(row.address)
                    && row
                        .location
                        .as_ref()
                        .is_some_and(|location| location.file == file && location.line <= line)
            })
        };
        addresses
            .iter()
            .flat_map(|address| {
                self.code_range_index
                    .containing(*address)
                    .filter_map(|instance| self.code_instance(instance))
            })
            .filter(|instance| matches!(instance.kind, CodeInstanceKind::OutOfLine))
            .any(encloses_request)
            .then_some(*next)
    }

    /// Finds the single function with the supplied source-level name.
    ///
    /// Only functions with code compete: a compile unit that merely calls
    /// a function defined in another one may describe it by a declaration.
    pub fn function_named(&self, name: &str) -> Result<&FunctionInfo> {
        let named = self.functions_named(name).collect::<Vec<_>>();
        let defined = named
            .iter()
            .copied()
            .filter(|function| self.instances_for_function(function.id).next().is_some())
            .collect::<Vec<_>>();
        match (defined.as_slice(), named.as_slice()) {
            ([function], _) | ([], [function]) => Ok(function),
            (_, []) => Err(Error::FunctionNotFound(name.to_owned())),
            _ => Err(Error::DuplicateFunction(name.to_owned())),
        }
    }

    /// Returns every function with the supplied source-level name, such as
    /// C++ overloads and same-named static functions of different files.
    pub fn functions_named(&self, name: &str) -> impl Iterator<Item = &FunctionInfo> {
        self.functions_by_name
            .get(name)
            .into_iter()
            .flat_map(|functions| functions.iter())
            .map(|function| {
                self.function(*function)
                    .expect("name index references a function")
            })
    }

    /// Every linker symbol with the supplied name.
    pub fn symbols_named(&self, name: &str) -> impl Iterator<Item = &SymbolInfo> {
        self.symbols_by_name
            .get(name)
            .into_iter()
            .flat_map(|symbols| symbols.iter())
            .filter_map(|symbol| self.symbol(*symbol))
    }

    /// Finds the single linker symbol with the supplied name.
    pub fn symbol_named(&self, name: &str) -> Result<&SymbolInfo> {
        let matches = self
            .symbols_by_name
            .get(name)
            .ok_or_else(|| Error::SymbolNotFound(name.to_owned()))?;
        let [symbol] = matches.as_ref() else {
            return Err(Error::DuplicateSymbol(name.to_owned()));
        };

        Ok(self
            .symbol(*symbol)
            .expect("name index references a symbol"))
    }

    /// What the code at an image address is to unwinding and stepping: the
    /// role of the physical function containing it, or else of the code
    /// symbol naming it, or else ordinary code.
    #[must_use]
    pub fn code_role(&self, address: ImageAddress) -> CodeRole {
        let physical = self
            .code_range_index
            .containing(address)
            .filter_map(|instance| self.code_instance(instance))
            .filter(|instance| matches!(instance.kind, CodeInstanceKind::OutOfLine))
            .min_by_key(|instance| instance.id);
        if let Some(function) = physical.and_then(|instance| self.function(instance.function)) {
            return function.role;
        }
        self.symbolize(address)
            .and_then(|location| self.symbol(location.symbol))
            .map_or(CodeRole::Ordinary, |symbol| symbol.role)
    }

    /// Resolves an image address to its available function and source metadata.
    #[must_use]
    pub fn locate(&self, address: ImageAddress) -> ImageLocation {
        let physical = self
            .code_range_index
            .containing(address)
            .filter_map(|instance| self.code_instance(instance))
            .filter(|instance| matches!(instance.kind, CodeInstanceKind::OutOfLine))
            .min_by_key(|instance| instance.id);
        let inline_frames = self.inline_frames(address, physical.map(|instance| instance.id));
        let logical_instance = match &inline_frames {
            InlineFrameLookup::Unique(chain) => chain.instances.last().copied(),
            InlineFrameLookup::None | InlineFrameLookup::Ambiguous(_) => None,
        };
        let function_id = logical_instance
            .and_then(|instance| self.code_instance(instance))
            .map(|instance| instance.function)
            .or_else(|| physical.map(|instance| instance.function));
        let function = function_id
            .and_then(|function_id| self.function(function_id))
            .cloned();
        let source = self
            .line_entry_containing(address)
            .map(|entry| entry.location.clone());

        ImageLocation {
            address,
            function,
            physical_instance: physical.map(|instance| instance.id),
            inline_frames,
            source,
            symbol: self.symbolize(address),
        }
    }

    fn inline_frames(
        &self,
        address: ImageAddress,
        physical: Option<CodeInstanceId>,
    ) -> InlineFrameLookup {
        let mut chains = Vec::new();

        for instance in self
            .code_range_index
            .containing(address)
            .filter_map(|instance| self.code_instance(instance))
            .filter(|instance| {
                matches!(
                    instance.kind,
                    CodeInstanceKind::Inline { call_site: Some(_) }
                )
            })
        {
            if let Some(chain) = self.inline_chain(instance.id, address, physical)
                && !chains.contains(&chain)
            {
                chains.push(chain);
            }
        }

        let chains: Vec<_> = chains
            .iter()
            .filter(|candidate| {
                !chains.iter().any(|other| {
                    candidate.len() < other.len() && other.starts_with(candidate.as_slice())
                })
            })
            .cloned()
            .map(|instances| InlineChain {
                instances: instances.into(),
            })
            .collect();

        match chains.len() {
            0 => InlineFrameLookup::None,
            1 => InlineFrameLookup::Unique(chains.into_iter().next().expect("one chain")),
            _ => InlineFrameLookup::Ambiguous(chains.into()),
        }
    }

    fn inline_chain(
        &self,
        mut instance: CodeInstanceId,
        address: ImageAddress,
        physical: Option<CodeInstanceId>,
    ) -> Option<Vec<CodeInstanceId>> {
        let mut chain = Vec::new();

        loop {
            let current = self.code_instance(instance)?;

            if !current.contains(address) {
                return None;
            }
            match &current.kind {
                CodeInstanceKind::Inline { call_site: Some(_) } => chain.push(current.id),
                CodeInstanceKind::Inline { call_site: None } => return None,
                CodeInstanceKind::OutOfLine => {
                    if Some(current.id) != physical {
                        return None;
                    }
                    break;
                }
            }
            instance = current.parent?;
        }

        chain.reverse();
        Some(chain)
    }

    /// Looks up a source file by its identifier.
    #[must_use]
    pub fn source_file(&self, id: SourceFileId) -> Option<&SourceFile> {
        self.source_files.get(id.index())
    }
}

/// Orders the code symbols containing one address from most to least
/// preferred; see [`ModuleImage::symbolize`].
fn symbol_preference(symbol: &SymbolInfo) -> impl Ord + '_ {
    let extent = symbol.extent.expect("indexed symbols have extents");
    (
        extent.provenance,
        std::cmp::Reverse(extent.range.start),
        extent.range.end.get() - extent.range.start.get(),
        symbol.kind,
        symbol.binding,
        !symbol.exported,
        symbol.name.bytes().take_while(|byte| *byte == b'_').count(),
        symbol.name.as_ref(),
        symbol.id,
    )
}

/// Collects the addresses debug information and code symbols prove begin
/// instructions, keeping the strongest evidence for each address.
fn instruction_starts(metadata: &ModuleMetadata) -> Arc<[(ImageAddress, crate::BoundaryEvidence)]> {
    let mut starts = BTreeMap::new();
    let functions = metadata
        .code_instances
        .iter()
        .filter(|instance| matches!(instance.kind, CodeInstanceKind::OutOfLine))
        .flat_map(|instance| instance.ranges.iter())
        .map(|range| (range.start, crate::BoundaryEvidence::FunctionRange));
    let symbols = metadata
        .symbols
        .iter()
        .filter_map(|symbol| symbol.extent)
        .map(|extent| (extent.range.start, crate::BoundaryEvidence::CodeSymbol));
    let sections = metadata
        .sections
        .iter()
        .filter(|section| section.executable)
        .map(|section| (section.range.start, crate::BoundaryEvidence::SectionStart));
    for (address, evidence) in functions.chain(symbols).chain(sections) {
        starts
            .entry(address)
            .and_modify(|current: &mut crate::BoundaryEvidence| *current = (*current).min(evidence))
            .or_insert(evidence);
    }
    starts.into_iter().collect()
}

/// Orders the data symbols naming one address, preferring the innermost
/// storage and then the names [`symbol_preference`] prefers.
fn storage_preference(symbol: &SymbolInfo) -> impl Ord + '_ {
    let storage = symbol.storage.expect("indexed symbols have storage");
    (
        std::cmp::Reverse(storage.start),
        storage.end.get() - storage.start.get(),
        symbol.binding,
        !symbol.exported,
        symbol.name.bytes().take_while(|byte| *byte == b'_').count(),
        symbol.name.as_ref(),
        symbol.id,
    )
}

/// Matches an absolute path exactly and a relative path as a suffix of whole
/// components, ignoring `.` components such as a leading `./`.
fn path_matches(candidate: &Path, requested: &Path) -> bool {
    fn significant(path: &Path) -> Vec<std::path::Component<'_>> {
        path.components()
            .filter(|component| *component != std::path::Component::CurDir)
            .collect()
    }

    if requested.is_absolute() {
        return candidate == requested;
    }
    let (candidate, requested) = (significant(candidate), significant(requested));
    !requested.is_empty() && candidate.ends_with(&requested)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Architecture, BaseType, BaseTypeEncoding, ByteOrder, GlobalVariableType,
        GlobalVariableVisibility, LineSequenceId, PointerWidth, StatementFlags, SymbolBinding,
        SymbolExtent, TypeId, TypeKind,
    };

    fn test_image(end: u64, metadata: ModuleMetadata) -> ModuleImage {
        ModuleImage::new(
            PathBuf::from("/test"),
            TargetDescription {
                architecture: Architecture::X86_64,
                byte_order: ByteOrder::Little,
                pointer_width: PointerWidth::Bits64,
            },
            AddressRange {
                start: ImageAddress::new(0),
                end: ImageAddress::new(end),
            },
            metadata,
        )
    }

    fn functions(names: &[&str]) -> Vec<FunctionInfo> {
        names
            .iter()
            .enumerate()
            .map(|(id, name)| FunctionInfo {
                id: FunctionId::new(u32::try_from(id).expect("small function count")),
                name: (*name).into(),
                linkage_name: None,
                declaration: None,
                language: crate::SourceLanguage::C,
                role: CodeRole::Ordinary,
                enclosing: None,
            })
            .collect()
    }

    #[test]
    fn global_indexes_support_exact_qualification_and_structured_ambiguity() {
        let int = TypeInfo {
            reference: TypeReference {
                image: ModuleImageId::new(0),
                id: TypeId::new(0),
            },
            name: "int".into(),
            byte_size: Some(4),
            kind: TypeKind::Base(BaseType {
                name: "int".into(),
                base_name: "int".into(),
                encoding: BaseTypeEncoding::Signed,
                byte_size: 4,
                bit_size: None,
            }),
            identity: None,
        };
        let globals = [
            ("left::shared", "_ZL11left_shared", 0),
            ("right::shared", "_ZL12right_shared", 1),
        ]
        .into_iter()
        .enumerate()
        .map(
            |(id, (qualified_name, linkage_name, file))| GlobalVariableInfo {
                id: GlobalVariableId::new(u32::try_from(id).expect("small global count")),
                name: "shared".into(),
                qualified_name: qualified_name.into(),
                linkage_name: Some(linkage_name.into()),
                declaration: Some(SourceLocation {
                    file: SourceFileId::new(file),
                    line: LineNumber::new(7).expect("nonzero line"),
                    column: None,
                }),
                type_info: GlobalVariableType::Resolved(int.clone()),
                visibility: GlobalVariableVisibility::CompilationUnit,
            },
        )
        .collect();
        let image = test_image(
            1,
            ModuleMetadata {
                globals,
                source_files: ["/build/src/left.c", "/build/src/right.c"]
                    .into_iter()
                    .enumerate()
                    .map(|(id, path)| SourceFile {
                        id: SourceFileId::new(u32::try_from(id).expect("small file count")),
                        path: Arc::new(PathBuf::from(path)),
                    })
                    .collect(),
                ..ModuleMetadata::default()
            },
        );

        let selected = |selector| image.global_named(selector).expect(selector).id;
        assert_eq!(selected("left::shared"), GlobalVariableId::new(0));
        assert_eq!(selected("right.c::right::shared"), GlobalVariableId::new(1));
        assert_eq!(selected("_ZL11left_shared"), GlobalVariableId::new(0));
        let Err(Error::AmbiguousGlobalVariable {
            selector,
            candidates,
        }) = image.global_named("shared")
        else {
            panic!("the basename is not ambiguous");
        };
        assert_eq!(selector, "shared");
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| (candidate.id, candidate.qualified_name.as_ref()))
                .collect::<Vec<_>>(),
            [
                (GlobalVariableId::new(0), "left::shared"),
                (GlobalVariableId::new(1), "right::shared"),
            ]
        );
    }

    #[test]
    fn module_type_graph_resolves_only_owned_dense_references() {
        let image_id = ModuleImageId::new(7);
        let reference = |image, id| TypeReference {
            image,
            id: TypeId::new(id),
        };
        let image = test_image(
            1,
            ModuleMetadata {
                types: Arc::from([
                    TypeNode::Resolved(TypeInfo {
                        reference: reference(image_id, 0),
                        name: "int".into(),
                        byte_size: Some(4),
                        kind: TypeKind::Opaque {
                            description: "test type".into(),
                        },
                        identity: None,
                    }),
                    TypeNode::Malformed {
                        reference: reference(image_id, 1),
                        description: "bad type".into(),
                    },
                ]),
                ..ModuleMetadata::default()
            },
        )
        .with_id(image_id);

        assert_eq!(
            image
                .type_info(reference(image_id, 0))
                .map(|info| &*info.name),
            Some("int")
        );
        assert!(matches!(
            image.type_node(reference(image_id, 1)),
            Some(TypeNode::Malformed { description, .. }) if &**description == "bad type"
        ));
        assert!(image.type_info(reference(image_id, 1)).is_none());
        assert!(
            image
                .type_node(reference(ModuleImageId::new(8), 0))
                .is_none()
        );
        assert!(image.type_node(reference(image_id, 2)).is_none());
    }

    #[test]
    fn source_path_matching_uses_whole_trailing_components() {
        let candidate = Path::new("/build/project/src/main.c");
        assert!(path_matches(candidate, Path::new("main.c")));
        assert!(path_matches(candidate, Path::new("src/main.c")));
        assert!(path_matches(candidate, Path::new("./main.c")));
        assert!(path_matches(candidate, Path::new("src/./main.c")));
        assert!(!path_matches(candidate, Path::new(".")));
        assert!(path_matches(candidate, candidate));
        assert!(!path_matches(candidate, Path::new("rc/main.c")));
        assert!(!path_matches(candidate, Path::new("other/main.c")));
    }

    fn source(line: u64) -> SourceLocation {
        SourceLocation {
            file: SourceFileId::new(0),
            line: LineNumber::new(line).expect("nonzero line"),
            column: None,
        }
    }

    fn instance(
        id: u32,
        function: u32,
        parent: Option<u32>,
        kind: CodeInstanceKind,
        ranges: &[(u64, u64)],
    ) -> CodeInstanceInfo {
        CodeInstanceInfo {
            id: CodeInstanceId::new(id),
            function: FunctionId::new(function),
            parent: parent.map(CodeInstanceId::new),
            kind,
            ranges: ranges
                .iter()
                .map(|&(start, end)| AddressRange {
                    start: ImageAddress::new(start),
                    end: ImageAddress::new(end),
                })
                .collect::<Vec<_>>()
                .into(),
            breakpoint_entry: None,
        }
    }

    fn inline_test_image(code_instances: Vec<CodeInstanceInfo>) -> ModuleImage {
        test_image(
            100,
            ModuleMetadata {
                functions: functions(&["physical", "middle", "leaf", "sibling"]),
                code_instances,
                ..ModuleMetadata::default()
            },
        )
    }

    #[test]
    fn instruction_starts_come_from_out_of_line_ranges_code_symbols_and_code_sections() {
        use crate::BoundaryEvidence::{CodeSymbol, FunctionRange, SectionStart};

        let code_symbol = |id, name: &str, start, end| SymbolInfo {
            id: SymbolId::new(id),
            name: name.into(),
            address: ImageAddress::new(start),
            kind: SymbolKind::Function,
            binding: SymbolBinding::Global,
            exported: true,
            extent: Some(SymbolExtent {
                range: AddressRange {
                    start: ImageAddress::new(start),
                    end: ImageAddress::new(end),
                },
                provenance: SymbolExtentProvenance::Declared,
            }),
            storage: None,
            role: CodeRole::Ordinary,
        };
        let section = |id, name: &str, start, end, executable| SectionInfo {
            id: SectionId::new(id),
            name: name.into(),
            range: AddressRange {
                start: ImageAddress::new(start),
                end: ImageAddress::new(end),
            },
            executable,
            writable: !executable,
        };
        let image = test_image(
            0x100,
            ModuleMetadata {
                functions: functions(&["split"]),
                code_instances: vec![
                    // A function split into two ranges, and an inline
                    // expansion whose start proves nothing on its own.
                    instance(
                        0,
                        0,
                        None,
                        CodeInstanceKind::OutOfLine,
                        &[(0x20, 0x30), (0x60, 0x68)],
                    ),
                    instance(
                        1,
                        0,
                        Some(0),
                        CodeInstanceKind::Inline { call_site: None },
                        &[(0x24, 0x28)],
                    ),
                ],
                // A symbol at a function's start is weaker evidence than
                // the function itself.
                symbols: vec![
                    code_symbol(0, "split", 0x20, 0x30),
                    code_symbol(1, "symbol_only", 0x40, 0x48),
                ],
                sections: vec![
                    section(0, ".text", 0x10, 0x70, true),
                    section(1, ".data", 0x80, 0x90, false),
                ],
                ..ModuleMetadata::default()
            },
        );
        let starts = |start, end| {
            image
                .instruction_starts(AddressRange {
                    start: ImageAddress::new(start),
                    end: ImageAddress::new(end),
                })
                .map(|(address, evidence)| (address.get(), evidence))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            starts(0, 0x100),
            [
                (0x10, SectionStart),
                (0x20, FunctionRange),
                (0x40, CodeSymbol),
                (0x60, FunctionRange),
            ]
        );
        assert_eq!(
            starts(0x20, 0x60),
            [(0x20, FunctionRange), (0x40, CodeSymbol)]
        );
    }

    #[test]
    fn control_boundaries_keep_exact_rows_and_enter_physical_instances_after_prologues() {
        let row = |address, location, flags, sequence, ordinal| StatementRow {
            address: ImageAddress::new(address),
            operation_index: 0,
            location,
            discriminator: 0,
            flags,
            isa: 0,
            sequence: LineSequenceId::new(sequence),
            ordinal,
        };
        let entry = |address, provenance| BreakpointEntry {
            address: ImageAddress::new(address),
            provenance,
        };
        let mut physical = instance(0, 0, None, CodeInstanceKind::OutOfLine, &[(0x10, 0x30)]);
        physical.breakpoint_entry = Some(entry(0x10, EntryProvenance::Explicit));
        let mut inline = instance(
            1,
            1,
            Some(0),
            CodeInstanceKind::Inline {
                call_site: Some(source(7)),
            },
            &[(0x14, 0x20)],
        );
        inline.breakpoint_entry = Some(entry(0x14, EntryProvenance::RangeStart));
        let flags = StatementFlags::empty;
        let image = test_image(
            0x40,
            ModuleMetadata {
                functions: functions(&["physical", "inline"]),
                code_instances: vec![physical, inline],
                statements: vec![
                    row(
                        0x14,
                        None,
                        flags().with_statement(true).with_prologue_end(true),
                        0,
                        2,
                    ),
                    row(
                        0x14,
                        Some(source(8)),
                        flags().with_epilogue_begin(true),
                        0,
                        3,
                    ),
                    row(0x18, Some(source(9)), flags().with_prologue_end(true), 1, 0),
                    row(0x20, None, flags().with_epilogue_begin(true), 1, 1),
                ],
                ..ModuleMetadata::default()
            },
        );

        let exact = image
            .control_boundaries_at(ImageAddress::new(0x14))
            .map(|row| (row.sequence, row.ordinal, row.location.is_some()))
            .collect::<Vec<_>>();
        assert_eq!(
            exact,
            [
                (LineSequenceId::new(0), 2, false),
                (LineSequenceId::new(0), 3, true)
            ]
        );
        assert!(
            image
                .control_boundaries_at(ImageAddress::new(0x15))
                .next()
                .is_none()
        );
        let entries = |id| {
            image
                .recommended_entries_for_instance(CodeInstanceId::new(id))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            entries(0),
            [
                entry(0x14, EntryProvenance::Statement),
                entry(0x18, EntryProvenance::Statement)
            ]
        );
        // Inline instances have no prologue of their own.
        assert_eq!(entries(1), [entry(0x14, EntryProvenance::RangeStart)]);
        assert!(
            image
                .statement_addresses(SourceFileId::new(0), LineNumber::new(8).unwrap())
                .next()
                .is_none()
        );
    }

    #[test]
    fn inline_lookup_preserves_nested_discontiguous_ranges_and_boundaries() {
        let image = inline_test_image(vec![
            instance(0, 0, None, CodeInstanceKind::OutOfLine, &[(0, 100)]),
            instance(
                1,
                1,
                Some(0),
                CodeInstanceKind::Inline {
                    call_site: Some(source(10)),
                },
                &[(20, 30), (40, 50)],
            ),
            instance(
                2,
                2,
                Some(1),
                CodeInstanceKind::Inline {
                    call_site: Some(source(20)),
                },
                &[(22, 25)],
            ),
            instance(
                3,
                3,
                Some(0),
                CodeInstanceKind::Inline { call_site: None },
                &[(60, 70)],
            ),
        ]);

        let InlineFrameLookup::Unique(nested) = image.locate(ImageAddress::new(22)).inline_frames
        else {
            panic!("nested inline chain was not unique")
        };
        assert_eq!(
            nested.instances.as_ref(),
            &[CodeInstanceId::new(1), CodeInstanceId::new(2)]
        );
        assert!(matches!(
            image.locate(ImageAddress::new(40)).inline_frames,
            InlineFrameLookup::Unique(_)
        ));
        for address in [30, 35, 50, 60] {
            assert_eq!(
                image.locate(ImageAddress::new(address)).inline_frames,
                InlineFrameLookup::None,
                "unexpected inline frame at {address}"
            );
        }
    }

    #[test]
    fn overlapping_sibling_inline_instances_are_explicitly_ambiguous() {
        let image = inline_test_image(vec![
            instance(0, 0, None, CodeInstanceKind::OutOfLine, &[(0, 100)]),
            instance(
                1,
                1,
                Some(0),
                CodeInstanceKind::Inline {
                    call_site: Some(source(10)),
                },
                &[(20, 30)],
            ),
            instance(
                2,
                2,
                Some(0),
                CodeInstanceKind::Inline {
                    call_site: Some(source(11)),
                },
                &[(25, 35)],
            ),
        ]);

        let InlineFrameLookup::Ambiguous(chains) =
            image.locate(ImageAddress::new(26)).inline_frames
        else {
            panic!("overlapping siblings were not reported as ambiguous")
        };
        let mut chains = chains
            .iter()
            .map(|chain| chain.instances.as_ref())
            .collect::<Vec<_>>();
        chains.sort_unstable();
        assert_eq!(chains, [[CodeInstanceId::new(1)], [CodeInstanceId::new(2)]]);
    }

    /// One test symbol: name, start, end (equal for a symbol without an
    /// extent), provenance, kind, binding, and whether it is exported.
    type TestSymbol = (
        &'static str,
        u64,
        u64,
        SymbolExtentProvenance,
        SymbolKind,
        SymbolBinding,
        bool,
    );

    fn symbol_test_image(symbols: &[TestSymbol]) -> ModuleImage {
        sectioned_symbol_test_image(symbols, &[])
    }

    /// Builds an image of symbols and sections, each section given by name,
    /// start, end, and whether it is executable.
    fn sectioned_symbol_test_image(
        symbols: &[TestSymbol],
        sections: &[(&'static str, u64, u64, bool)],
    ) -> ModuleImage {
        let sections = sections
            .iter()
            .enumerate()
            .map(|(index, &(name, start, end, executable))| SectionInfo {
                id: SectionId::new(u32::try_from(index).expect("test section count")),
                name: name.into(),
                range: AddressRange {
                    start: ImageAddress::new(start),
                    end: ImageAddress::new(end),
                },
                executable,
                writable: !executable,
            })
            .collect();
        let symbols = symbols
            .iter()
            .enumerate()
            .map(
                |(index, &(name, start, end, provenance, kind, binding, exported))| SymbolInfo {
                    id: SymbolId::new(u32::try_from(index).expect("test symbol count")),
                    name: name.into(),
                    address: ImageAddress::new(start),
                    kind,
                    binding,
                    exported,
                    extent: (start < end && kind != SymbolKind::Data).then_some(SymbolExtent {
                        range: AddressRange {
                            start: ImageAddress::new(start),
                            end: ImageAddress::new(end),
                        },
                        provenance,
                    }),
                    storage: (kind == SymbolKind::Data).then_some(AddressRange {
                        start: ImageAddress::new(start),
                        end: ImageAddress::new(end),
                    }),
                    role: CodeRole::Ordinary,
                },
            )
            .collect();
        test_image(
            0x1000,
            ModuleMetadata {
                symbols,
                sections,
                ..ModuleMetadata::default()
            },
        )
    }

    fn symbolized(image: &ModuleImage, address: u64) -> Option<(&str, u64)> {
        image.symbolize(ImageAddress::new(address)).map(|location| {
            (
                image.symbol(location.symbol).expect("known").name.as_ref(),
                location.offset,
            )
        })
    }

    #[test]
    fn symbolization_uses_only_containing_extents_and_prefers_declared_innermost_code() {
        use SymbolBinding::{Global, Local};
        use SymbolExtentProvenance::{Declared, Inferred};
        use SymbolKind::{Data, Function};

        let image = symbol_test_image(&[
            ("outer", 0x100, 0x200, Declared, Function, Global, true),
            ("inner", 0x140, 0x160, Declared, Function, Local, false),
            // An unsized label inside a sized function never displaces it.
            ("label", 0x180, 0x190, Inferred, Function, Global, true),
            ("tail", 0x300, 0x340, Inferred, Function, Local, false),
            // Symbols without an extent never name code.
            ("object", 0x240, 0x240, Declared, Data, Global, true),
        ]);

        assert_eq!(symbolized(&image, 0xff), None);
        assert_eq!(symbolized(&image, 0x100), Some(("outer", 0)));
        assert_eq!(symbolized(&image, 0x13f), Some(("outer", 0x3f)));
        assert_eq!(symbolized(&image, 0x140), Some(("inner", 0)));
        assert_eq!(symbolized(&image, 0x15f), Some(("inner", 0x1f)));
        assert_eq!(symbolized(&image, 0x160), Some(("outer", 0x60)));
        assert_eq!(symbolized(&image, 0x185), Some(("outer", 0x85)));
        assert_eq!(symbolized(&image, 0x1ff), Some(("outer", 0xff)));
        // No nearest preceding symbol is guessed for unnamed code.
        assert_eq!(symbolized(&image, 0x200), None);
        assert_eq!(symbolized(&image, 0x240), None);
        assert_eq!(symbolized(&image, 0x33f), Some(("tail", 0x3f)));
        assert_eq!(
            image
                .symbolize(ImageAddress::new(0x300))
                .map(|location| location.provenance),
            Some(Inferred)
        );
        assert_eq!(symbolized(&image, 0x340), None);
    }

    #[test]
    fn same_extent_aliases_resolve_by_kind_binding_export_and_spelling() {
        use SymbolBinding::{Global, Local, Weak};
        use SymbolExtentProvenance::Declared;
        use SymbolKind::{Function, IndirectFunction};

        // Each row adds a candidate that outranks every earlier one by
        // exactly one rule and sorts after them by name, so only that rule
        // can select it.
        let ladder: [TestSymbol; 6] = [
            (
                "_a_resolver",
                0x10,
                0x20,
                Declared,
                IndirectFunction,
                Global,
                true,
            ),
            ("_b_local", 0x10, 0x20, Declared, Function, Local, false),
            ("_c_weak", 0x10, 0x20, Declared, Function, Weak, false),
            ("_d_hidden", 0x10, 0x20, Declared, Function, Global, false),
            ("_e_exported", 0x10, 0x20, Declared, Function, Global, true),
            ("f_exported", 0x10, 0x20, Declared, Function, Global, true),
        ];
        for count in 1..=ladder.len() {
            let image = symbol_test_image(&ladder[..count]);
            assert_eq!(
                symbolized(&image, 0x18).map(|(name, _)| name),
                Some(ladder[count - 1].0),
                "{count} candidates"
            );
        }

        // With every rule tied, the bytewise-smallest name wins regardless
        // of catalog order.
        let tied = symbol_test_image(&[
            ("beta", 0x10, 0x20, Declared, Function, Global, true),
            ("alpha", 0x10, 0x20, Declared, Function, Global, true),
        ]);
        assert_eq!(symbolized(&tied, 0x10), Some(("alpha", 0)));
    }

    #[test]
    fn descriptions_prefer_code_then_declared_storage_and_never_guess_a_neighbor() {
        use SymbolBinding::{Global, Local};
        use SymbolExtentProvenance::{Declared, Inferred};
        use SymbolKind::{Data, Function};

        let image = sectioned_symbol_test_image(
            &[
                ("function", 0x100, 0x140, Declared, Function, Global, true),
                // A data object inside code never displaces the function.
                ("table", 0x120, 0x130, Declared, Data, Global, true),
                ("outer", 0x800, 0x840, Declared, Data, Global, true),
                ("inner", 0x810, 0x818, Declared, Data, Local, false),
                // Unsized data names only its own address.
                ("label", 0x880, 0x880, Declared, Data, Global, true),
                ("inside", 0x820, 0x820, Declared, Data, Global, true),
            ],
            &[
                (".text", 0x100, 0x200, true),
                (".data", 0x800, 0x900, false),
                // A malformed overlapping section loses to the innermost one.
                (".overlap", 0x7f0, 0x8f0, false),
            ],
        );
        let described = |address: u64| {
            let description = image.describe(ImageAddress::new(address));
            (
                description
                    .section
                    .map(|section| (section.name.to_string(), section.offset)),
                description.symbol.map(|symbol| {
                    (
                        symbol.name.to_string(),
                        symbol.kind,
                        symbol.offset,
                        symbol.provenance,
                    )
                }),
            )
        };
        let text = |offset| Some((".text".to_owned(), offset));
        let data = |offset| Some((".data".to_owned(), offset));
        let symbol = |name: &str, kind, offset, provenance| {
            Some((name.to_owned(), kind, offset, provenance))
        };

        assert_eq!(
            described(0x124),
            (text(0x24), symbol("function", Function, 0x24, Declared))
        );
        assert_eq!(described(0x150), (text(0x50), None));
        assert_eq!(
            described(0x814),
            (data(0x14), symbol("inner", Data, 4, Declared))
        );
        assert_eq!(
            described(0x818),
            (data(0x18), symbol("outer", Data, 0x18, Declared))
        );
        // Sized storage wins over an unsized label at the same address.
        assert_eq!(
            described(0x820),
            (data(0x20), symbol("outer", Data, 0x20, Declared))
        );
        assert_eq!(described(0x840), (data(0x40), None));
        assert_eq!(
            described(0x880),
            (data(0x80), symbol("label", Data, 0, Inferred))
        );
        assert_eq!(described(0x881), (data(0x81), None));
        assert_eq!(described(0x7f8), (Some((".overlap".to_owned(), 8)), None));
        assert_eq!(described(0x900), (None, None));
    }
}
