//! A small image with every kind of row, which tests and the fuzz harness
//! change to see what validation makes of it.

use std::path::PathBuf;

use zerocopy::IntoBytes as _;

use super::facts::{self, FactsView};
use super::format::Trailer;
use super::functions::{self, FunctionView};
use super::lines::{
    self, FileRecord, LineExtra, LineRange, LineRow, LineSequence, LineTables, Row, RowAddress,
};
use super::packages::{self, PackageView};
use super::symbols::{self, SymbolView};
use super::types::{self, TypeView};
use super::unwind::{self, UnwindView};
use super::{Builder, Image, ImageError, Limits, PathId, Paths, StringsBuilder, TableKind};
use crate::{
    AddressRange, BoundaryEvidence, BreakpointEntry, CodeInstanceId, CodeInstanceInfo,
    CodeInstanceKind, CodeRole, ColumnNumber, EntryProvenance, FunctionId, FunctionInfo, GotSlot,
    GotTarget, ImageAddress, LineNumber, SectionId, SectionInfo, SourceFileId, SourceLanguage,
    SourceLocation, SymbolBinding, SymbolExtent, SymbolExtentProvenance, SymbolId, SymbolInfo,
    SymbolKind, TypeId,
};

pub(super) const TARGET: crate::TargetDescription = crate::TargetDescription::X86_64;

pub(super) fn row(address: u64, file: Option<u32>, line: u64) -> Row {
    Row {
        address,
        file: file.map(SourceFileId::new),
        line,
        column: 0,
        operation_index: 0,
        discriminator: 0,
        isa: 0,
        statement: true,
        prologue_end: false,
        epilogue_begin: false,
        end_sequence: false,
    }
}

pub(super) const fn range(start: u64, end: u64) -> AddressRange<ImageAddress> {
    AddressRange {
        start: ImageAddress::new(start),
        end: ImageAddress::new(end),
    }
}

/// Two sequences with every kind of row: wide lines and columns, extras,
/// rows without files, boundaries, and equal addresses.
pub(super) fn sample_rows() -> Vec<Vec<Row>> {
    vec![
        vec![
            row(0x1000, Some(0), 3),
            Row {
                prologue_end: true,
                column: 7,
                ..row(0x1004, Some(0), 4)
            },
            Row {
                discriminator: 2,
                statement: false,
                ..row(0x1004, Some(0), 5)
            },
            Row {
                line: u64::from(u32::MAX) + 9,
                column: 1 << 20,
                ..row(0x1008, Some(1), 0)
            },
            Row {
                epilogue_begin: true,
                ..row(0x100c, None, 0)
            },
            Row {
                end_sequence: true,
                ..row(0x1010, Some(1), 9)
            },
        ],
        vec![
            row(0x2000, Some(1), 1),
            Row {
                isa: 3,
                operation_index: 1,
                ..row(0x2000, Some(1), 2)
            },
            Row {
                end_sequence: true,
                ..row(0x2008, Some(1), 2)
            },
        ],
    ]
}

pub(super) fn sample() -> (LineTables, lines::Files) {
    let mut files = lines::Files::default();
    files.intern(PathBuf::from("/src/a.c"));
    files.intern(PathBuf::from(
        <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(b"/src/\xffb.c".to_vec()),
    ));
    let mut tables = LineTables::default();
    for sequence in sample_rows() {
        tables.begin_sequence().unwrap();
        let mut previous: Option<(u32, Row)> = None;
        for row in sequence {
            let at = tables.push_row(&row).unwrap();
            if let Some((index, open)) = previous.take()
                && open.location().is_some()
                && open.address < row.address
            {
                tables
                    .push_range(range(open.address, row.address), index, open.statement)
                    .unwrap();
            }
            previous = Some((at, row));
        }
    }
    (tables, files)
}

/// Symbols of every shape: code that overlaps, declared and inferred,
/// sized and unsized data, and a symbol that names neither.
pub(super) fn sample_symbols() -> Vec<SymbolInfo> {
    let symbol = |id, name: &str, address, kind| SymbolInfo {
        id: SymbolId::new(id),
        name: name.into(),
        address: ImageAddress::new(address),
        kind,
        binding: SymbolBinding::Global,
        exported: false,
        extent: None,
        storage: None,
        role: CodeRole::Ordinary,
    };
    let code = |range, provenance| Some(SymbolExtent { range, provenance });
    vec![
        SymbolInfo {
            exported: true,
            extent: code(range(0x1000, 0x1010), SymbolExtentProvenance::Declared),
            ..symbol(0, "main", 0x1000, SymbolKind::Function)
        },
        SymbolInfo {
            binding: SymbolBinding::Local,
            role: CodeRole::Wrapper,
            extent: code(range(0x1008, 0x1020), SymbolExtentProvenance::Inferred),
            ..symbol(1, "helper", 0x1008, SymbolKind::IndirectFunction)
        },
        SymbolInfo {
            binding: SymbolBinding::Weak,
            storage: Some(range(0x3000, 0x3008)),
            ..symbol(2, "counter", 0x3000, SymbolKind::Data)
        },
        SymbolInfo {
            storage: Some(range(0x3010, 0x3010)),
            ..symbol(3, "counter", 0x3010, SymbolKind::Data)
        },
        symbol(4, "memcpy@GLIBC_2.2.5", 0x1000, SymbolKind::Unknown),
    ]
}

pub(super) fn sample_sections() -> Vec<SectionInfo> {
    let section = |id, name: &str, range, executable| SectionInfo {
        id: SectionId::new(id),
        name: name.into(),
        range,
        executable,
        writable: !executable,
    };
    vec![
        section(0, ".text", range(0x1000, 0x2000), true),
        section(1, ".data", range(0x3000, 0x3100), false),
    ]
}

pub(super) fn sample_got() -> Vec<GotSlot> {
    vec![
        GotSlot {
            address: ImageAddress::new(0x3100),
            target: GotTarget::Import("puts".into()),
        },
        GotSlot {
            address: ImageAddress::new(0x3108),
            target: GotTarget::Indirect(ImageAddress::new(0x1008)),
        },
    ]
}

/// Functions of every shape: generic, enclosed, coroutine-running, of
/// another language, without code, and inlined.
pub(super) fn sample_functions() -> Vec<FunctionInfo> {
    let function = |id, name: &str| FunctionInfo {
        id: FunctionId::new(id),
        name: name.into(),
        linkage_name: None,
        declaration: None,
        language: SourceLanguage::C,
        role: CodeRole::Ordinary,
        enclosing: None,
        coroutine: None,
        generics: std::sync::Arc::from([]),
    };
    vec![
        FunctionInfo {
            linkage_name: Some("_Z4mainv".into()),
            declaration: Some(SourceLocation {
                file: SourceFileId::new(0),
                line: LineNumber::new(u64::from(u32::MAX) + 3).unwrap(),
                column: ColumnNumber::new(7),
            }),
            language: SourceLanguage::Rust,
            coroutine: Some(TypeId::new(4)),
            generics: [("T".into(), TypeId::new(2)), ("U".into(), TypeId::new(3))].into(),
            ..function(0, "main")
        },
        FunctionInfo {
            language: SourceLanguage::Other(0x8001),
            role: CodeRole::Wrapper,
            enclosing: Some(FunctionId::new(0)),
            ..function(1, "helper")
        },
        function(2, "declared"),
        function(3, "main"),
    ]
}

/// Instances of every shape: physical with two ranges, the same range
/// twice, and an inline expansion with a call site and without.
pub(super) fn sample_instances() -> Vec<CodeInstanceInfo> {
    let entry = |address, provenance| {
        Some(BreakpointEntry {
            address: ImageAddress::new(address),
            provenance,
        })
    };
    vec![
        CodeInstanceInfo {
            id: CodeInstanceId::new(0),
            function: FunctionId::new(0),
            parent: None,
            kind: CodeInstanceKind::OutOfLine,
            ranges: [range(0x1000, 0x1010), range(0x2000, 0x2008)].into(),
            breakpoint_entry: entry(0x1000, EntryProvenance::Explicit),
        },
        CodeInstanceInfo {
            id: CodeInstanceId::new(1),
            function: FunctionId::new(1),
            parent: Some(CodeInstanceId::new(0)),
            kind: CodeInstanceKind::Inline {
                call_site: Some(SourceLocation {
                    file: SourceFileId::new(1),
                    line: LineNumber::new(9).unwrap(),
                    column: None,
                }),
            },
            ranges: [range(0x1004, 0x1008), range(0x1004, 0x1008)].into(),
            breakpoint_entry: entry(0x1004, EntryProvenance::RangeStart),
        },
        CodeInstanceInfo {
            id: CodeInstanceId::new(2),
            function: FunctionId::new(3),
            parent: None,
            kind: CodeInstanceKind::Inline { call_site: None },
            ranges: [range(0x3000, 0x3004)].into(),
            breakpoint_entry: None,
        },
        CodeInstanceInfo {
            id: CodeInstanceId::new(3),
            function: FunctionId::new(0),
            parent: None,
            kind: CodeInstanceKind::OutOfLine,
            ranges: [range(0x4000, 0x4010)].into(),
            breakpoint_entry: entry(0x4008, EntryProvenance::CoroutineBody),
        },
    ]
}

pub(super) fn sample_starts() -> Vec<(ImageAddress, BoundaryEvidence)> {
    vec![
        (ImageAddress::new(0x1000), BoundaryEvidence::FunctionRange),
        (ImageAddress::new(0x2000), BoundaryEvidence::SectionStart),
    ]
}

/// Call-frame sections of arbitrary bytes with overlapping entries and a
/// malformed one, and Go's table.
pub(super) fn sample_unwind() -> unwind::Unwind {
    let index = |entries: &[(u64, u64, u32)], first_error: Option<(u64, &str)>| unwind::FdeIndex {
        entries: super::index::intervals(
            entries
                .iter()
                .map(|&(start, end, offset)| (range(start, end), offset)),
        ),
        first_error: first_error.map(|(offset, text)| (offset, text.to_owned())),
    };
    unwind::Unwind {
        eh_frame: (0..64).collect::<Vec<u8>>().into(),
        debug_frame: (0..32).collect::<Vec<u8>>().into(),
        eh_frame_index: index(
            &[
                (0x1000, 0x1100, 8),
                (0x1080, 0x1200, 24),
                (0x1400, 0x1500, 48),
            ],
            Some((40, "a CIE is malformed")),
        ),
        debug_frame_index: index(&[(0x2000, 0x2100, 0)], None),
        bases: unwind::Bases {
            eh_frame: Some(0x9000),
            text: Some(0x1000),
            got: None,
        },
        big_endian: false,
        address_size: 8,
        go_code: vec![range(0x3000, 0x3100), range(0x3200, 0x3300)],
        go: Some(unwind::GoTableData {
            bytes: (0..16).collect::<Vec<u8>>().into(),
            facts: unwind::GoTableFacts {
                address: 0x8000,
                text: 0x1000,
                go_func: Some(0x8800),
                release: Some((1, 25)),
            },
            frame_saves: vec![Some(0x3004..0x30f0), None],
        }),
    }
}

pub(super) fn sample_sources() -> crate::SymbolTableSources {
    crate::SymbolTableSources {
        static_table: true,
        dynamic_table: false,
        embedded_table: crate::EmbeddedSymbolTable::Unusable {
            reason: "the table is truncated".into(),
        },
        runtime_function_table: crate::EmbeddedSymbolTable::Loaded,
    }
}

pub(super) fn sample_thread_locals()
-> std::collections::BTreeMap<std::sync::Arc<str>, Result<crate::ThreadLocal, std::sync::Arc<str>>>
{
    [
        ("counter", Ok(crate::ThreadLocal::Offset(-16))),
        (
            "library_state",
            Ok(crate::ThreadLocal::Slot(ImageAddress::new(0x5000))),
        ),
        ("lost", Err("no code reads it".into())),
    ]
    .into_iter()
    .map(|(name, place)| (name.into(), place))
    .collect()
}

/// Packages, one named twice, whose last name counts.
pub(super) const SAMPLE_PACKAGES: [(&str, &str); 3] = [
    ("main", "first"),
    ("example.com/m/stack", "stack"),
    ("main", "main"),
];

/// Two of the sample functions' packages and local names, which are the
/// same.
pub(super) fn sample_packaged() -> Vec<Option<(&'static str, String)>> {
    vec![
        None,
        Some(("example.com/m/stack", "Push".to_owned())),
        None,
        Some(("main", "Push".to_owned())),
    ]
}

/// Types of every kind and shape, each at its identifier's index, and
/// their identity classes: the last type is the same as the second.
#[expect(clippy::too_many_lines, reason = "one type of each kind and shape")]
pub(super) fn sample_types() -> (Vec<crate::TypeNode>, Vec<u32>) {
    use crate::{
        Accessibility, ArgumentOrigin, ArrayDimension, BaseClass, BaseClassVirtuality, BaseType,
        BaseTypeEncoding, EnumerationOrigin, Enumerator, GoKind, GoTypeAttributes, IntegerValue,
        ModuleImageId, NamedTypeRelationship, RecordKind, RecordMember, RecordMemberLayout,
        ReferenceKind, TypeArgument, TypeIdentity, TypeInfo, TypeKind, TypeModifier, TypeNode,
        TypeReference, Variant, VariantDiscriminant, VariantSelection, VariantSelector,
        VariantStorageKind,
    };
    let reference = |id| TypeReference {
        image: ModuleImageId::new(0),
        id: TypeId::new(id),
    };
    let identity = |language, path: &[&str], base: &str| TypeIdentity {
        language,
        path: path.iter().map(|segment| (*segment).into()).collect(),
        inline_namespaces: [].into(),
        base: base.into(),
        arguments: [].into(),
        pack: None,
        origin: ArgumentOrigin::None,
        go: None,
    };
    let member = |name: Option<&str>, ty, layout| RecordMember {
        name: name.map(Into::into),
        type_ref: reference(ty),
        layout,
        accessibility: Accessibility::Public,
        artificial: false,
        embedded: false,
        declaration: None,
    };
    let int = BaseType {
        name: "int".into(),
        base_name: "int".into(),
        encoding: BaseTypeEncoding::Signed,
        byte_size: 4,
        bit_size: None,
    };
    let kinds: Vec<(&str, Option<u64>, TypeKind, Option<TypeIdentity>)> = vec![
        (
            "int",
            Some(4),
            TypeKind::Base(int),
            Some(identity(SourceLanguage::C, &[], "int")),
        ),
        (
            "int *",
            Some(8),
            TypeKind::Pointer {
                target: Some(reference(0)),
                address_class: 0,
            },
            None,
        ),
        (
            "geo::Point<int, -3, N>",
            Some(16),
            TypeKind::Record {
                kind: RecordKind::Class,
                members: [
                    RecordMember {
                        declaration: Some(SourceLocation {
                            file: SourceFileId::new(0),
                            line: LineNumber::new(3).unwrap(),
                            column: ColumnNumber::new(5),
                        }),
                        accessibility: Accessibility::Private,
                        ..member(Some("x"), 0, RecordMemberLayout::ByteOffset(0))
                    },
                    RecordMember {
                        artificial: true,
                        ..member(
                            Some("y"),
                            0,
                            RecordMemberLayout::BitRange {
                                bit_offset: 32,
                                bit_size: 16,
                            },
                        )
                    },
                    RecordMember {
                        embedded: true,
                        ..member(None, 1, RecordMemberLayout::Runtime)
                    },
                ]
                .into(),
                bases: [BaseClass {
                    type_ref: reference(3),
                    layout: RecordMemberLayout::ByteOffset(8),
                    accessibility: Accessibility::Protected,
                    virtuality: BaseClassVirtuality::Virtual,
                }]
                .into(),
                incomplete: false,
            },
            Some(TypeIdentity {
                inline_namespaces: ["__1".into()].into(),
                arguments: [
                    TypeArgument::Type(reference(0)),
                    TypeArgument::Value(IntegerValue::Signed(-3)),
                    TypeArgument::Unknown("N".into()),
                ]
                .into(),
                pack: Some(1),
                origin: ArgumentOrigin::Dwarf,
                ..identity(SourceLanguage::Cpp, &["geo", "__1"], "Point")
            }),
        ),
        (
            "shapes::Shape",
            Some(8),
            TypeKind::Variant {
                storage: VariantStorageKind::Struct,
                common_members: [member(Some("tag"), 0, RecordMemberLayout::ByteOffset(0))].into(),
                bases: [].into(),
                discriminant: Box::new(VariantDiscriminant::Stored(member(
                    Some("tag"),
                    0,
                    RecordMemberLayout::ByteOffset(0),
                ))),
                variants: [
                    Variant {
                        name: Some("Circle".into()),
                        selection: VariantSelection::Selectors(
                            [
                                VariantSelector::Value(IntegerValue::Unsigned(u128::MAX)),
                                VariantSelector::Range {
                                    low: IntegerValue::Signed(i128::MIN),
                                    high: IntegerValue::Signed(5),
                                },
                            ]
                            .into(),
                        ),
                        members: [member(Some("r"), 0, RecordMemberLayout::ByteOffset(4))].into(),
                    },
                    Variant {
                        name: None,
                        selection: VariantSelection::Default,
                        members: [].into(),
                    },
                ]
                .into(),
                incomplete: false,
            },
            Some(identity(SourceLanguage::Rust, &["shapes"], "Shape")),
        ),
        (
            "main.Color",
            Some(4),
            TypeKind::Enumeration {
                representation: BaseType {
                    name: "main.Color".into(),
                    base_name: "unsigned int".into(),
                    encoding: BaseTypeEncoding::Unsigned,
                    byte_size: 4,
                    bit_size: Some(3),
                },
                underlying: Some(reference(0)),
                enumerators: [("Red", 0), ("Green", 1), ("Red", 2)]
                    .into_iter()
                    .map(|(name, value)| Enumerator {
                        name: name.into(),
                        value: IntegerValue::Unsigned(value),
                    })
                    .collect(),
                origin: EnumerationOrigin::NamedConstants,
                scoped: true,
            },
            Some(TypeIdentity {
                go: Some(GoTypeAttributes {
                    kind: GoKind::Uint,
                    runtime_type: Some(0x100),
                }),
                ..identity(SourceLanguage::Go, &["main"], "Color")
            }),
        ),
        (
            "[3][4]int",
            Some(48),
            TypeKind::Array {
                element: reference(0),
                dimensions: [
                    ArrayDimension {
                        lower_bound: -1,
                        count: 3,
                    },
                    ArrayDimension {
                        lower_bound: i128::MAX,
                        count: 4,
                    },
                ]
                .into(),
            },
            None,
        ),
        (
            "[]int",
            Some(24),
            TypeKind::Slice {
                element: reference(0),
                has_capacity: true,
                text: false,
            },
            None,
        ),
        (
            "union U",
            None,
            TypeKind::Union {
                members: [
                    member(Some("a"), 0, RecordMemberLayout::ByteOffset(0)),
                    member(Some("b"), 1, RecordMemberLayout::ByteOffset(0)),
                ]
                .into(),
                incomplete: true,
            },
            None,
        ),
        (
            "const int",
            Some(4),
            TypeKind::Modified {
                modifier: TypeModifier::Const,
                target: reference(0),
            },
            None,
        ),
        (
            "main.Alias",
            Some(4),
            TypeKind::Named {
                target: Some(reference(4)),
                relationship: NamedTypeRelationship::Distinct,
            },
            Some(TypeIdentity {
                go: Some(GoTypeAttributes {
                    kind: GoKind::Other(99),
                    runtime_type: Some(0x100),
                }),
                ..identity(SourceLanguage::Go, &["main"], "Alias")
            }),
        ),
        ("void", None, TypeKind::Unspecified, None),
        ("func()", Some(8), TypeKind::Function, None),
        (
            "int (int, int *, ...)",
            None,
            TypeKind::Signature {
                returns: Some(reference(0)),
                parameters: [reference(0), reference(1)].into(),
                variadic: true,
                prototyped: true,
            },
            None,
        ),
        (
            "member pointer",
            Some(16),
            TypeKind::Opaque {
                description: "DW_TAG_ptr_to_member_type".into(),
            },
            None,
        ),
        ("", None, TypeKind::Unspecified, None),
        (
            "Point &&",
            Some(8),
            TypeKind::Reference {
                kind: ReferenceKind::Rvalue,
                target: reference(2),
                address_class: 1,
            },
            None,
        ),
        (
            "Tagged",
            Some(4),
            TypeKind::Variant {
                storage: VariantStorageKind::Union,
                common_members: [].into(),
                bases: [].into(),
                discriminant: Box::new(VariantDiscriminant::TagType(reference(0))),
                variants: [].into(),
                incomplete: true,
            },
            None,
        ),
        (
            "Never",
            Some(0),
            TypeKind::Variant {
                storage: VariantStorageKind::Class,
                common_members: [].into(),
                bases: [].into(),
                discriminant: Box::new(VariantDiscriminant::Absent),
                variants: [].into(),
                incomplete: false,
            },
            None,
        ),
        (
            "short int",
            Some(2),
            TypeKind::Base(BaseType {
                name: "short int".into(),
                base_name: "short int".into(),
                encoding: BaseTypeEncoding::Signed,
                byte_size: 2,
                bit_size: None,
            }),
            Some(identity(SourceLanguage::C, &[], "short int")),
        ),
        (
            "int *",
            Some(8),
            TypeKind::Pointer {
                target: Some(reference(0)),
                address_class: 0,
            },
            None,
        ),
    ];
    let mut nodes = kinds
        .into_iter()
        .enumerate()
        .map(|(index, (name, byte_size, kind, identity))| {
            TypeNode::Resolved(TypeInfo {
                reference: reference(u32::try_from(index).unwrap()),
                name: name.into(),
                byte_size,
                kind,
                identity: identity.map(std::sync::Arc::new),
            })
        })
        .collect::<Vec<_>>();
    nodes[14] = TypeNode::Malformed {
        reference: reference(14),
        description: "the type's size is negative".into(),
    };
    let mut classes = (0..u32::try_from(nodes.len()).unwrap()).collect::<Vec<_>>();
    *classes.last_mut().unwrap() = 1;
    (nodes, classes)
}

pub(super) fn seal(tables: &LineTables, files: &lines::Files) -> Result<Image, ImageError> {
    let mut builder = Builder::new(TARGET);
    tables.add_to(&mut builder);
    files.add_to(&mut builder).unwrap();
    let mut strings = StringsBuilder::default();
    symbols::add_to(
        &mut builder,
        &mut strings,
        &sample_symbols(),
        &sample_sections(),
        &sample_got(),
    )
    .unwrap();
    functions::add_to(
        &mut builder,
        &mut strings,
        &functions::Code {
            functions: &sample_functions(),
            instances: &sample_instances(),
            prologue_ends: &tables.prologue_ends(),
            instruction_starts: &sample_starts(),
        },
    )
    .unwrap();
    unwind::add_to(&mut builder, &mut strings, &sample_unwind()).unwrap();
    let (nodes, classes) = sample_types();
    types::add_to(
        &mut builder,
        &mut strings,
        &types::Types {
            nodes: &nodes,
            classes: &classes,
        },
    )
    .unwrap();
    packages::add_to(
        &mut builder,
        &mut strings,
        SAMPLE_PACKAGES,
        &sample_packaged(),
    )
    .unwrap();
    facts::add_to(
        &mut builder,
        &mut strings,
        &facts::Facts {
            symbol_sources: &sample_sources(),
            thread_local_storage: true,
            thread_locals: &sample_thread_locals(),
        },
    )
    .unwrap();
    builder.bytes(TableKind::Strings, strings.into_bytes());
    builder.seal(Limits::default())
}

/// Reads everything a valid image's views can read, so that a test can
/// show none of it panics.
pub(super) fn read_everything(image: &Image) -> u64 {
    let addresses = image.table::<RowAddress>();
    let rows = image.table::<LineRow>();
    let extras = image.table::<LineExtra>();
    let mut read = 0_u64;
    for index in 0..rows.len() {
        read = read.wrapping_add(lines::decode_row(addresses, rows, extras, index).line);
    }
    read += lines::statement_rows(addresses, rows, extras, image.table::<LineSequence>()).count()
        as u64;
    for range in image.table::<LineRange>() {
        read = read.wrapping_add(
            lines::line_entry(addresses, rows, extras, range)
                .location
                .line
                .get(),
        );
    }
    let paths = Paths(image.bytes(TableKind::Paths));
    for file in image.table::<FileRecord>() {
        read += paths.get(PathId(file.path.get())).as_os_str().len() as u64;
    }
    let view = SymbolView::new(image);
    for symbol in view.all() {
        let info = symbol.info();
        read += view.named(&info.name).count() as u64;
        for address in [info.address.get(), symbol.range_end().get()] {
            let address = ImageAddress::new(address);
            read += u64::from(view.code_at(address).is_some());
            read += u64::from(view.data_at(address).is_some());
        }
        read += u64::from(symbol.answers_to("main"));
    }
    read += symbols::sections(image).len() as u64;
    read += symbols::got_slots(image).len() as u64;
    let view = FunctionView::new(image);
    for function in view.functions() {
        let info = function.info();
        read += view.named(&info.name).count() as u64;
        read += function
            .instances()
            .map(|instance| instance.info().ranges.len() as u64)
            .sum::<u64>();
    }
    for instance in view.instances() {
        let info = instance.info();
        read += instance.recommended_entries().count() as u64;
        for range in info.ranges.iter() {
            read += view.instances_containing(range.start).count() as u64;
            read += view.instruction_starts(*range).count() as u64;
        }
    }
    let view = UnwindView::new(image);
    for lookup in [view.eh_frame_index(), view.debug_frame_index()] {
        for interval in lookup.entries {
            for address in [interval.start.get(), interval.end.get() - 1] {
                read += u64::from(lookup.lookup(address).is_ok());
            }
        }
    }
    read += (view.eh_frame().len() + view.debug_frame().len()) as u64;
    read += u64::from(view.is_go_code(ImageAddress::new(0x3000)));
    read += view.frame_saves().flatten().count() as u64;
    if let Some((bytes, _)) = view.go() {
        read += bytes.len() as u64;
    }
    let view = TypeView::new(image);
    for index in 0..view.len() {
        let id = TypeId::new(u32::try_from(index).unwrap());
        if let Some(crate::TypeNode::Resolved(info)) = view.node(crate::ModuleImageId::new(0), id) {
            read += view.named(&info.name).count() as u64;
            read += u64::from(view.class(id).is_some());
            if let Some(identity) = &info.identity {
                read += view.with_base(&identity.base).count() as u64;
            }
            if let crate::TypeKind::Enumeration { enumerators, .. } = &info.kind {
                for enumerator in enumerators.iter() {
                    read += view.with_enumerator(&enumerator.name).count() as u64;
                }
            }
        }
    }
    read += u64::from(view.go_runtime_type(0x100).is_some());
    let view = PackageView::new(image);
    for index in 0..image.table::<packages::PackagedRecord>().len() {
        let function = FunctionId::new(u32::try_from(index).unwrap());
        if let Some((package, local)) = view.packaged_name(function) {
            read += view.with_local_name(local).count() as u64;
            read += u64::from(view.package_name(package).is_some());
        }
    }
    let view = FactsView::new(image);
    read += u64::from(view.thread_local_storage());
    read += u64::from(view.symbol_sources().static_table);
    for (name, _) in view.thread_locals() {
        read += u64::from(view.thread_local(name).is_some());
    }
    read
}

pub(super) fn reseal(bytes: &mut [u8]) {
    let end = bytes.len() - size_of::<Trailer>();
    let checksum = twox_hash::XxHash3_64::oneshot(&bytes[..end]);
    bytes[end..].copy_from_slice(
        Trailer {
            checksum: checksum.into(),
        }
        .as_bytes(),
    );
}

/// Validates `data` as an image and reads everything a valid one holds.
/// Odd-first-byte inputs are changes to the sample: (offset, byte) pairs
/// `XORed` into it. Every input is resealed with a correct checksum, so that
/// the fuzzer reaches the checks past it.
#[cfg(feature = "fuzzing")]
pub fn fuzz(data: &[u8]) {
    let Some((&mode, data)) = data.split_first() else {
        return;
    };
    let mut bytes = if mode & 1 == 1 {
        let (tables, files) = sample();
        let mut bytes = seal(&tables, &files)
            .expect("the sample is valid")
            .as_bytes()
            .to_vec();
        for [low, high, flip] in data.as_chunks::<3>().0 {
            let at = usize::from(u16::from_le_bytes([*low, *high])) % bytes.len();
            bytes[at] ^= flip;
        }
        bytes
    } else {
        data.to_vec()
    };
    if bytes.len() >= size_of::<Trailer>() {
        reseal(&mut bytes);
    }
    let Some(mut aligned) = super::AlignedBytes::zeroed(bytes.len()) else {
        return;
    };
    aligned.as_mut_bytes().copy_from_slice(&bytes);
    if let Ok(image) = Image::from_bytes(aligned, Limits::default()) {
        read_everything(&image);
        let view = lines::LineView::new(&image);
        for row in view.statement_rows() {
            let _ = view.control_boundaries_at(row.address).count();
            let _ = view.line_entry_containing(row.address);
            if let Some(location) = row.location {
                let _ = view.statements(location.file, 0..=location.line.get());
            }
        }
    }
}
