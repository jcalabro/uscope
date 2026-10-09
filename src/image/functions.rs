//! Source-level functions, the code instances that place them in the
//! image, and the indexes that find them: by name, by function, and by
//! address. Each instance carries the entries a function breakpoint
//! prefers, and the image the addresses where instructions are known to
//! begin.

use zerocopy::little_endian::{U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::index::{self, Interval, NameEntry};
use super::strings::{StrId, Strings, StringsBuilder};
use super::symbols::{role, role_code, valid_role};
use super::{Builder, Image, NONE, Record, SharedRecord, TableKind};
use crate::{
    AddressRange, BoundaryEvidence, BreakpointEntry, CodeInstanceId, CodeInstanceInfo,
    CodeInstanceKind, CodeRole, ColumnNumber, EntryProvenance, FunctionId, FunctionInfo,
    ImageAddress, LineNumber, SourceFileId, SourceLanguage, SourceLocation, TypeId,
};

/// A source location: a file, or [`NONE`] for none, and its line and
/// column, each zero when absent.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct LocationRecord {
    pub file: U32,
    pub line: U64,
    pub column: U64,
}

impl LocationRecord {
    const NONE: Self = Self {
        file: U32::new(NONE),
        line: U64::new(0),
        column: U64::new(0),
    };

    fn of(location: Option<&SourceLocation>) -> Self {
        location.map_or(Self::NONE, |location| Self {
            file: location.file.get().into(),
            line: location.line.get().into(),
            column: location.column.map_or(0, ColumnNumber::get).into(),
        })
    }

    fn get(&self) -> Option<SourceLocation> {
        (self.file.get() != NONE).then(|| SourceLocation {
            file: SourceFileId::new(self.file.get()),
            line: LineNumber::new(self.line.get()).expect("validation checked the line"),
            column: ColumnNumber::new(self.column.get()),
        })
    }

    fn valid(&self, files: usize) -> bool {
        if self.file.get() == NONE {
            *self == Self::NONE
        } else {
            (self.file.get() as usize) < files && self.line.get() != 0
        }
    }
}

/// One source-level function.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct FunctionRecord {
    pub name: U32,
    /// The linkage name, or [`NONE`].
    pub linkage_name: U32,
    pub declaration: LocationRecord,
    /// The function whose loop this one is the body of, or [`NONE`].
    pub enclosing: U32,
    /// The type of the coroutine the function runs, or [`NONE`].
    pub coroutine: U32,
    /// The function's first type argument in [`TableKind::Generics`].
    pub generics: U32,
    pub generic_count: U32,
    /// The function's first instance in [`TableKind::FunctionInstances`].
    pub instances: U32,
    pub instance_count: U32,
    /// A language that has no code of its own, by its DWARF code.
    pub other_language: U16,
    pub language: u8,
    pub role: u8,
}

impl Record for FunctionRecord {
    const KIND: TableKind = TableKind::Functions;
}

/// One type argument of a generic function, with its parameter's name.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct GenericRecord {
    pub name: U32,
    pub argument: U32,
}

impl Record for GenericRecord {
    const KIND: TableKind = TableKind::Generics;
}

/// One concrete placement of a function in the image.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct InstanceRecord {
    pub function: U32,
    /// The nearest instance containing an inline expansion, or [`NONE`].
    pub parent: U32,
    /// An inline expansion's call, when described.
    pub call_site: LocationRecord,
    /// The instance's first range in [`TableKind::InstanceRanges`].
    pub ranges: U32,
    pub range_count: U32,
    /// The instance's first recommended entry in
    /// [`TableKind::RecommendedEntries`].
    pub entries: U32,
    pub entry_count: U32,
    /// The preferred function breakpoint location, when `provenance` is
    /// not [`NONE_PROVENANCE`].
    pub entry: U64,
    pub provenance: u8,
    pub inline: u8,
}

impl Record for InstanceRecord {
    const KIND: TableKind = TableKind::CodeInstances;
}

/// An address range of an instance.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct RangeRecord {
    pub start: U64,
    pub end: U64,
}

impl Record for RangeRecord {
    const KIND: TableKind = TableKind::InstanceRanges;
}

impl RangeRecord {
    const fn get(&self) -> AddressRange<ImageAddress> {
        AddressRange {
            start: ImageAddress::new(self.start.get()),
            end: ImageAddress::new(self.end.get()),
        }
    }
}

/// A breakpoint location, and how it was chosen.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct EntryRecord {
    pub address: U64,
    pub provenance: u8,
}

impl Record for EntryRecord {
    const KIND: TableKind = TableKind::RecommendedEntries;
}

/// An address where an instruction is known to begin, and the best
/// evidence of it.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct StartRecord {
    pub address: U64,
    pub evidence: u8,
}

impl Record for StartRecord {
    const KIND: TableKind = TableKind::InstructionStarts;
}

/// A record's number, as one index lists them.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Member {
    pub value: U32,
}

impl SharedRecord for Member {
    const NAME: &'static str = "Member";
}

const _: () = assert!(size_of::<LocationRecord>() == 20);
const _: () = assert!(size_of::<FunctionRecord>() == 56);
const _: () = assert!(size_of::<InstanceRecord>() == 54);

/// [`InstanceRecord::provenance`] for an instance without an entry.
pub const NONE_PROVENANCE: u8 = u8::MAX;

const PROVENANCES: [EntryProvenance; 5] = [
    EntryProvenance::Explicit,
    EntryProvenance::Statement,
    EntryProvenance::AnalyzedPrologue,
    EntryProvenance::RangeStart,
    EntryProvenance::CoroutineBody,
];
const EVIDENCE: [BoundaryEvidence; 5] = [
    BoundaryEvidence::ProgramCounter,
    BoundaryEvidence::FunctionRange,
    BoundaryEvidence::CodeSymbol,
    BoundaryEvidence::SectionStart,
    BoundaryEvidence::RangeEnd,
];
const LANGUAGES: [SourceLanguage; 7] = [
    SourceLanguage::C,
    SourceLanguage::Cpp,
    SourceLanguage::Rust,
    SourceLanguage::Go,
    SourceLanguage::Zig,
    SourceLanguage::Other(0),
    SourceLanguage::Unknown,
];
const OTHER_LANGUAGE: u8 = 5;

fn code_of<T: PartialEq>(known: &[T], value: &T) -> u8 {
    u8::try_from(
        known
            .iter()
            .position(|candidate| candidate == value)
            .expect("every value has a code"),
    )
    .expect("few values")
}

fn language_code(language: SourceLanguage) -> (u8, u16) {
    match language {
        SourceLanguage::Other(code) => (OTHER_LANGUAGE, code),
        language => (code_of(&LANGUAGES, &language), 0),
    }
}

/// A count or index of records, which the builder keeps below [`NONE`].
fn number(count: usize) -> Result<u32, TooMany> {
    u32::try_from(count)
        .ok()
        .filter(|count| *count < NONE)
        .ok_or(TooMany)
}

fn optional(id: Option<u32>) -> U32 {
    id.unwrap_or(NONE).into()
}

/// Why functions could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the functions, their names, or their instances do not fit an image")]
pub struct TooMany;

/// What [`add_to`] encodes.
pub struct Code<'a> {
    pub functions: &'a [FunctionInfo],
    pub instances: &'a [CodeInstanceInfo],
    /// Where the line tables end prologues, in line-table order, which
    /// orders each instance's recommended entries.
    pub prologue_ends: &'a [ImageAddress],
    /// Where instructions are known to begin, by address, once each.
    pub instruction_starts: &'a [(ImageAddress, BoundaryEvidence)],
}

/// Adds the functions, their instances, and their indexes to `builder`,
/// pooling names in `strings`.
pub fn add_to(
    builder: &mut Builder,
    strings: &mut StringsBuilder,
    code: &Code<'_>,
) -> Result<(), TooMany> {
    add_functions(builder, strings, code.functions, code.instances)?;
    add_instances(builder, code.instances, code.prologue_ends)?;
    let starts = code
        .instruction_starts
        .iter()
        .map(|(address, evidence)| StartRecord {
            address: address.get().into(),
            evidence: code_of(&EVIDENCE, evidence),
        })
        .collect::<Vec<_>>();
    builder.table(&starts);
    Ok(())
}

fn add_functions(
    builder: &mut Builder,
    strings: &mut StringsBuilder,
    functions: &[FunctionInfo],
    instances: &[CodeInstanceInfo],
) -> Result<(), TooMany> {
    // Each function's instances, in order.
    let mut by_function = instances
        .iter()
        .map(|instance| (instance.function.get(), instance.id.get()))
        .collect::<Vec<_>>();
    by_function.sort_unstable();
    let members = by_function
        .iter()
        .map(|(_, instance)| Member {
            value: (*instance).into(),
        })
        .collect::<Vec<_>>();

    let mut records = Vec::with_capacity(functions.len());
    let mut names = Vec::with_capacity(functions.len());
    let mut generics = Vec::new();
    let mut first_instance = 0;
    for function in functions {
        let name = strings.push(&function.name).ok_or(TooMany)?;
        names.push((&*function.name, name, function.id.get()));
        let first_generic = number(generics.len())?;
        for (parameter, argument) in function.generics.iter() {
            generics.push(GenericRecord {
                name: strings.push(parameter).ok_or(TooMany)?.0.into(),
                argument: argument.get().into(),
            });
        }
        let instance_count =
            by_function[first_instance..].partition_point(|(owner, _)| *owner == function.id.get());
        let (language, other_language) = language_code(function.language);
        records.push(FunctionRecord {
            name: name.0.into(),
            linkage_name: match &function.linkage_name {
                Some(name) => strings.push(name).ok_or(TooMany)?.0.into(),
                None => NONE.into(),
            },
            declaration: LocationRecord::of(function.declaration.as_ref()),
            enclosing: optional(function.enclosing.map(FunctionId::get)),
            coroutine: optional(function.coroutine.map(TypeId::get)),
            generics: first_generic.into(),
            generic_count: number(function.generics.len())?.into(),
            instances: number(first_instance)?.into(),
            instance_count: number(instance_count)?.into(),
            other_language: other_language.into(),
            language,
            role: role_code(function.role),
        });
        first_instance += instance_count;
    }
    number(generics.len())?;
    builder
        .table(&records)
        .shared(TableKind::FunctionNames, &index::names(names))
        .table(&generics)
        .shared(TableKind::FunctionInstances, &members);
    Ok(())
}

fn add_instances(
    builder: &mut Builder,
    instances: &[CodeInstanceInfo],
    prologue_ends: &[ImageAddress],
) -> Result<(), TooMany> {
    let code_ranges = index::intervals(instances.iter().flat_map(|instance| {
        instance
            .ranges
            .iter()
            .map(|range| (*range, instance.id.get()))
    }));
    let entries = recommended_entries(instances, &code_ranges, prologue_ends);
    let mut instance_records = Vec::with_capacity(instances.len());
    let mut ranges = Vec::new();
    let mut entry_records = Vec::new();
    for (instance, recommended) in instances.iter().zip(&entries) {
        let (call_site, inline) = match &instance.kind {
            CodeInstanceKind::OutOfLine => (None, 0),
            CodeInstanceKind::Inline { call_site } => (call_site.as_ref(), 1),
        };
        instance_records.push(InstanceRecord {
            function: instance.function.get().into(),
            parent: optional(instance.parent.map(CodeInstanceId::get)),
            call_site: LocationRecord::of(call_site),
            ranges: number(ranges.len())?.into(),
            range_count: number(instance.ranges.len())?.into(),
            entries: number(entry_records.len())?.into(),
            entry_count: number(recommended.len())?.into(),
            entry: instance
                .breakpoint_entry
                .map_or(0, |entry| entry.address.get())
                .into(),
            provenance: instance.breakpoint_entry.map_or(NONE_PROVENANCE, |entry| {
                code_of(&PROVENANCES, &entry.provenance)
            }),
            inline,
        });
        ranges.extend(instance.ranges.iter().map(|range| RangeRecord {
            start: range.start.get().into(),
            end: range.end.get().into(),
        }));
        entry_records.extend(recommended.iter().map(|entry| EntryRecord {
            address: entry.address.get().into(),
            provenance: code_of(&PROVENANCES, &entry.provenance),
        }));
    }
    number(ranges.len())?;
    number(entry_records.len())?;
    builder
        .table(&instance_records)
        .table(&ranges)
        .shared(TableKind::CodeRanges, &code_ranges)
        .table(&entry_records);
    Ok(())
}

/// The entries a breakpoint on each instance prefers: an out-of-line
/// instance's prologue ends, or else its own entry. A coroutine's body
/// begins past its dispatch, wherever its prologue ends.
fn recommended_entries(
    instances: &[CodeInstanceInfo],
    code_ranges: &[Interval],
    prologue_ends: &[ImageAddress],
) -> Vec<Vec<BreakpointEntry>> {
    let mut prologues = vec![Vec::new(); instances.len()];
    for address in prologue_ends {
        for id in index::containing(code_ranges, *address) {
            let entries = &mut prologues[id as usize];
            if matches!(instances[id as usize].kind, CodeInstanceKind::OutOfLine)
                && !entries
                    .iter()
                    .any(|entry: &BreakpointEntry| entry.address == *address)
            {
                entries.push(BreakpointEntry {
                    address: *address,
                    provenance: EntryProvenance::Statement,
                });
            }
        }
    }
    instances
        .iter()
        .zip(prologues)
        .map(|(instance, prologues)| match instance.breakpoint_entry {
            Some(entry) if entry.provenance == EntryProvenance::CoroutineBody => vec![entry],
            _ if !prologues.is_empty() => prologues,
            entry => entry.into_iter().collect(),
        })
        .collect()
}

/// The functions and instances of a validated image.
#[derive(Clone, Copy)]
pub struct FunctionView<'a> {
    strings: Strings<'a>,
    functions: &'a [FunctionRecord],
    names: &'a [NameEntry],
    generics: &'a [GenericRecord],
    members: &'a [Member],
    instances: &'a [InstanceRecord],
    ranges: &'a [RangeRecord],
    code_ranges: &'a [Interval],
    entries: &'a [EntryRecord],
    starts: &'a [StartRecord],
}

impl<'a> FunctionView<'a> {
    pub fn new(image: &'a Image) -> Self {
        Self {
            strings: image.strings(),
            functions: image.table(),
            names: image.shared(TableKind::FunctionNames),
            generics: image.table(),
            members: image.shared(TableKind::FunctionInstances),
            instances: image.table(),
            ranges: image.table(),
            code_ranges: image.shared(TableKind::CodeRanges),
            entries: image.table(),
            starts: image.table(),
        }
    }

    pub fn function(self, id: FunctionId) -> Option<Function<'a>> {
        Some(Function {
            view: self,
            id,
            record: self.functions.get(id.index())?,
        })
    }

    pub fn functions(self) -> impl ExactSizeIterator<Item = Function<'a>> + DoubleEndedIterator {
        (0..self.functions.len()).map(move |index| {
            self.function(FunctionId::new(
                u32::try_from(index).expect("function counts fit u32"),
            ))
            .expect("in range")
        })
    }

    /// The functions named `name`, in identifier order.
    pub fn named(self, name: &str) -> impl Iterator<Item = Function<'a>> + 'a {
        index::named(self.strings, self.names, name).map(move |id| {
            self.function(FunctionId::new(id))
                .expect("validation checked the name index")
        })
    }

    pub fn instance(self, id: CodeInstanceId) -> Option<CodeInstance<'a>> {
        Some(CodeInstance {
            view: self,
            id,
            record: self.instances.get(id.index())?,
        })
    }

    pub fn instances(
        self,
    ) -> impl ExactSizeIterator<Item = CodeInstance<'a>> + DoubleEndedIterator {
        (0..self.instances.len()).map(move |index| {
            self.instance(CodeInstanceId::new(
                u32::try_from(index).expect("instance counts fit u32"),
            ))
            .expect("in range")
        })
    }

    /// The instances whose code contains `address`, latest start first.
    pub fn instances_containing(
        self,
        address: ImageAddress,
    ) -> impl Iterator<Item = CodeInstance<'a>> + 'a {
        index::containing(self.code_ranges, address).map(move |id| {
            self.instance(CodeInstanceId::new(id))
                .expect("validation checked the code ranges")
        })
    }

    /// The known instruction starts within `range`, in order.
    pub fn instruction_starts(
        self,
        range: AddressRange<ImageAddress>,
    ) -> impl Iterator<Item = (ImageAddress, BoundaryEvidence)> + 'a {
        let first = self
            .starts
            .partition_point(|start| start.address.get() < range.start.get());
        self.starts[first..]
            .iter()
            .take_while(move |start| start.address.get() < range.end.get())
            .map(|start| {
                (
                    ImageAddress::new(start.address.get()),
                    EVIDENCE[usize::from(start.evidence)],
                )
            })
    }
}

/// One source-level function of a validated image.
#[derive(Clone, Copy)]
pub struct Function<'a> {
    view: FunctionView<'a>,
    id: FunctionId,
    record: &'a FunctionRecord,
}

impl std::fmt::Debug for Function<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.info().fmt(formatter)
    }
}

impl PartialEq for Function<'_> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.record, other.record)
    }
}

impl Eq for Function<'_> {}

impl<'a> Function<'a> {
    #[must_use]
    pub const fn id(self) -> FunctionId {
        self.id
    }

    /// The source-level function name.
    #[must_use]
    pub fn name(self) -> &'a str {
        self.view.strings.get(StrId(self.record.name.get()))
    }

    /// The linker-visible function name, when known.
    #[must_use]
    pub fn linkage_name(self) -> Option<&'a str> {
        let name = self.record.linkage_name.get();
        (name != NONE).then(|| self.view.strings.get(StrId(name)))
    }

    /// The function's declaration location, when known.
    #[must_use]
    pub fn declaration(self) -> Option<SourceLocation> {
        self.record.declaration.get()
    }

    /// The language of the unit that defines the function.
    #[must_use]
    pub fn language(self) -> SourceLanguage {
        match self.record.language {
            OTHER_LANGUAGE => SourceLanguage::Other(self.record.other_language.get()),
            code => LANGUAGES[usize::from(code)],
        }
    }

    /// What the function is to unwinding and stepping.
    #[must_use]
    pub fn role(self) -> CodeRole {
        role(self.record.role)
    }

    /// The function whose loop this one is the body of, as
    /// [`FunctionInfo::enclosing`] describes.
    #[must_use]
    pub fn enclosing(self) -> Option<FunctionId> {
        let id = self.record.enclosing.get();
        (id != NONE).then(|| FunctionId::new(id))
    }

    /// The type of the coroutine the function runs, as
    /// [`FunctionInfo::coroutine`] describes.
    #[must_use]
    pub fn coroutine(self) -> Option<TypeId> {
        let id = self.record.coroutine.get();
        (id != NONE).then(|| TypeId::new(id))
    }

    /// A generic function's type arguments, each with its parameter's
    /// name.
    #[must_use]
    pub fn generics(self) -> impl ExactSizeIterator<Item = (&'a str, TypeId)> + 'a {
        let first = self.record.generics.get() as usize;
        let count = self.record.generic_count.get() as usize;
        let strings = self.view.strings;
        self.view.generics[first..first + count]
            .iter()
            .map(move |generic| {
                (
                    strings.get(StrId(generic.name.get())),
                    TypeId::new(generic.argument.get()),
                )
            })
    }

    /// The function's concrete instances, in order.
    #[must_use]
    pub fn instances(self) -> impl ExactSizeIterator<Item = CodeInstance<'a>> + 'a {
        let first = self.record.instances.get() as usize;
        let count = self.record.instance_count.get() as usize;
        let view = self.view;
        self.view.members[first..first + count]
            .iter()
            .map(move |member| {
                view.instance(CodeInstanceId::new(member.value.get()))
                    .expect("validation checked the instance index")
            })
    }

    /// The function as a record of its own.
    #[must_use]
    pub fn info(self) -> FunctionInfo {
        FunctionInfo {
            id: self.id,
            name: self.name().into(),
            linkage_name: self.linkage_name().map(Into::into),
            declaration: self.declaration(),
            language: self.language(),
            role: self.role(),
            enclosing: self.enclosing(),
            coroutine: self.coroutine(),
            generics: self
                .generics()
                .map(|(name, argument)| (name.into(), argument))
                .collect(),
        }
    }
}

/// One concrete placement of a function in a validated image.
#[derive(Clone, Copy)]
pub struct CodeInstance<'a> {
    view: FunctionView<'a>,
    id: CodeInstanceId,
    record: &'a InstanceRecord,
}

impl std::fmt::Debug for CodeInstance<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.info().fmt(formatter)
    }
}

impl PartialEq for CodeInstance<'_> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.record, other.record)
    }
}

impl Eq for CodeInstance<'_> {}

impl<'a> CodeInstance<'a> {
    #[must_use]
    pub const fn id(self) -> CodeInstanceId {
        self.id
    }

    /// The source-level function the instance places.
    #[must_use]
    pub const fn function(self) -> FunctionId {
        FunctionId::new(self.record.function.get())
    }

    /// The nearest containing instance, for an inline expansion.
    #[must_use]
    pub fn parent(self) -> Option<CodeInstanceId> {
        let id = self.record.parent.get();
        (id != NONE).then(|| CodeInstanceId::new(id))
    }

    /// Whether the instance is physical or inlined.
    #[must_use]
    pub fn kind(self) -> CodeInstanceKind {
        if self.record.inline == 0 {
            CodeInstanceKind::OutOfLine
        } else {
            CodeInstanceKind::Inline {
                call_site: self.record.call_site.get(),
            }
        }
    }

    /// Whether the instance is a physical, independently callable body.
    #[must_use]
    pub const fn is_out_of_line(self) -> bool {
        self.record.inline == 0
    }

    /// Every image-address range the instance occupies.
    pub fn ranges(
        self,
    ) -> impl ExactSizeIterator<Item = AddressRange<ImageAddress>> + DoubleEndedIterator + Clone + 'a
    {
        let first = self.record.ranges.get() as usize;
        let count = self.record.range_count.get() as usize;
        self.view.ranges[first..first + count]
            .iter()
            .map(RangeRecord::get)
    }

    /// Whether the instance contains an image address.
    #[must_use]
    pub fn contains(self, address: ImageAddress) -> bool {
        self.ranges().any(|range| range.contains(address))
    }

    /// The preferred location for a function breakpoint, when one exists.
    #[must_use]
    pub fn breakpoint_entry(self) -> Option<BreakpointEntry> {
        PROVENANCES
            .get(usize::from(self.record.provenance))
            .map(|provenance| BreakpointEntry {
                address: ImageAddress::new(self.record.entry.get()),
                provenance: *provenance,
            })
    }

    /// Where a function breakpoint stops in the instance: an out-of-line
    /// instance's every prologue end, or otherwise its breakpoint entry.
    #[must_use]
    pub fn recommended_entries(self) -> impl ExactSizeIterator<Item = BreakpointEntry> + 'a {
        let first = self.record.entries.get() as usize;
        let count = self.record.entry_count.get() as usize;
        self.view.entries[first..first + count]
            .iter()
            .map(|entry| BreakpointEntry {
                address: ImageAddress::new(entry.address.get()),
                provenance: PROVENANCES[usize::from(entry.provenance)],
            })
    }

    /// The instance as a record of its own.
    #[must_use]
    pub fn info(self) -> CodeInstanceInfo {
        CodeInstanceInfo {
            id: self.id,
            function: self.function(),
            parent: self.parent(),
            kind: self.kind(),
            ranges: self.ranges().collect(),
            breakpoint_entry: self.breakpoint_entry(),
        }
    }
}

/// Whether `count` records from `first` lie within `length`.
fn span(first: U32, count: U32, length: usize) -> bool {
    (first.get() as usize)
        .checked_add(count.get() as usize)
        .is_some_and(|end| end <= length)
}

/// Checks the function and instance tables and their indexes.
pub(super) fn validate(image: &Image) -> Result<(), String> {
    validate_functions(image)?;
    validate_instances(image)
}

fn validate_functions(image: &Image) -> Result<(), String> {
    let strings = image.strings();
    let files = image.table::<super::lines::FileRecord>().len();
    let functions = image.table::<FunctionRecord>();
    let generics = image.table::<GenericRecord>();
    let members = image.shared::<Member>(TableKind::FunctionInstances);
    let instances = image.table::<InstanceRecord>();

    // Each function's instances follow the last one's, so together they
    // list every member once.
    let mut next_member = 0;
    let mut next_generic = 0;
    for (index, function) in functions.iter().enumerate() {
        let language = usize::from(function.language);
        if !strings.contains(StrId(function.name.get()))
            || (function.linkage_name.get() != NONE
                && !strings.contains(StrId(function.linkage_name.get())))
            || !function.declaration.valid(files)
            || (function.enclosing.get() != NONE
                && function.enclosing.get() as usize >= functions.len())
            || language >= LANGUAGES.len()
            || (function.language != OTHER_LANGUAGE && function.other_language.get() != 0)
            || !valid_role(function.role)
            || function.generics.get() != next_generic
            || !span(function.generics, function.generic_count, generics.len())
            || function.instances.get() as usize != next_member
            || !span(function.instances, function.instance_count, members.len())
        {
            return Err("a function is malformed".into());
        }
        next_generic += function.generic_count.get();
        next_member += function.instance_count.get() as usize;
        let first = function.instances.get() as usize;
        let own = &members[first..first + function.instance_count.get() as usize];
        if !own.iter().all(|member| {
            instances
                .get(member.value.get() as usize)
                .is_some_and(|instance| instance.function.get() as usize == index)
        }) || !own.is_sorted_by(|earlier, later| earlier.value.get() < later.value.get())
        {
            return Err("the instance index disagrees with the instances".into());
        }
    }
    if next_generic as usize != generics.len()
        || next_member != members.len()
        || members.len() != instances.len()
        || !generics
            .iter()
            .all(|generic| strings.contains(StrId(generic.name.get())))
    {
        return Err("the function indexes do not cover their tables".into());
    }
    let names = image.shared::<NameEntry>(TableKind::FunctionNames);
    if names.len() != functions.len()
        || !index::valid_names(&strings, names, functions.len())
        || !names
            .iter()
            .all(|entry| functions[entry.value.get() as usize].name == entry.name)
    {
        return Err("the function name index disagrees with the functions".into());
    }
    Ok(())
}

fn validate_instances(image: &Image) -> Result<(), String> {
    let files = image.table::<super::lines::FileRecord>().len();
    let functions = image.table::<FunctionRecord>();
    let instances = image.table::<InstanceRecord>();
    let ranges = image.table::<RangeRecord>();
    let entries = image.table::<EntryRecord>();
    let mut next_range = 0;
    let mut next_entry = 0;
    for (index, instance) in instances.iter().enumerate() {
        let inline = match instance.inline {
            0 => false,
            1 => true,
            _ => return Err("an instance is malformed".into()),
        };
        if instance.function.get() as usize >= functions.len()
            || (instance.parent.get() != NONE && instance.parent.get() as usize >= index)
            || !instance.call_site.valid(files)
            || (!inline && instance.call_site != LocationRecord::NONE)
            || instance.ranges.get() != next_range
            || !span(instance.ranges, instance.range_count, ranges.len())
            || instance.entries.get() != next_entry
            || !span(instance.entries, instance.entry_count, entries.len())
            || (instance.provenance == NONE_PROVENANCE && instance.entry.get() != 0)
            || (instance.provenance != NONE_PROVENANCE
                && usize::from(instance.provenance) >= PROVENANCES.len())
        {
            return Err("an instance is malformed".into());
        }
        next_range += instance.range_count.get();
        next_entry += instance.entry_count.get();
    }
    if next_range as usize != ranges.len()
        || next_entry as usize != entries.len()
        || !ranges
            .iter()
            .all(|range| range.start.get() < range.end.get())
        || !entries
            .iter()
            .all(|entry| usize::from(entry.provenance) < PROVENANCES.len())
    {
        return Err("an instance's ranges or entries are malformed".into());
    }
    // Each entry is one of its instance's ranges, and each range has one.
    let code_ranges = image.shared::<Interval>(TableKind::CodeRanges);
    let own = |instance: &InstanceRecord| {
        let first = instance.ranges.get() as usize;
        &ranges[first..first + instance.range_count.get() as usize]
    };
    let indexed = |range: &RangeRecord, id: usize| {
        let key = (range.start.get(), range.end.get(), u32::try_from(id).ok());
        code_ranges
            .binary_search_by_key(&key, |interval| {
                (
                    interval.start.get(),
                    interval.end.get(),
                    Some(interval.value.get()),
                )
            })
            .is_ok()
    };
    if !index::valid_intervals(code_ranges, instances.len())
        || !code_ranges.iter().all(|interval| {
            own(&instances[interval.value.get() as usize])
                .iter()
                .any(|range| range.start == interval.start && range.end == interval.end)
        })
        || !instances
            .iter()
            .enumerate()
            .all(|(id, instance)| own(instance).iter().all(|range| indexed(range, id)))
    {
        return Err("the code range index disagrees with the instances".into());
    }
    let starts = image.table::<StartRecord>();
    if !starts
        .iter()
        .all(|start| usize::from(start.evidence) < EVIDENCE.len())
        || !starts.is_sorted_by(|earlier, later| earlier.address.get() < later.address.get())
    {
        return Err("the instruction starts are malformed".into());
    }
    Ok(())
}
