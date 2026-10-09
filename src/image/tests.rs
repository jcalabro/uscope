use super::facts::{FactsRecord, FactsView, ThreadLocalRecord};
use super::format::Header;
use super::functions::{
    EntryRecord, FunctionRecord, FunctionView, GenericRecord, InstanceRecord, Member, RangeRecord,
    StartRecord,
};
use super::index::{Interval, NameEntry};
use super::lines::{
    self, ControlBoundary, FileRecord, LineExtra, LineRange, LineRow, LineSequence, LineTables,
    RowAddress, StatementKey, row_flags,
};
use super::packages::{PackageRecord, PackageView, PackagedRecord};
use super::sample::{
    SAMPLE_PACKAGES, TARGET, read_everything, reseal, sample, sample_functions, sample_got,
    sample_instances, sample_packaged, sample_rows, sample_sections, sample_sources,
    sample_symbols, sample_thread_locals, sample_unwind, seal,
};
use super::symbols::{GotRecord, SectionRecord, SymbolRecord, SymbolView};
use super::unwind::{FdeMiss, FrameSaveRecord, UnwindRecord, UnwindView, unwind_flags};
use super::*;
use crate::{
    AddressRange, BreakpointEntry, CodeInstanceId, EntryProvenance, FunctionId, ImageAddress,
    SymbolId,
};

fn reopen(bytes: &[u8]) -> Result<Image, ImageError> {
    Image::from_bytes(AlignedBytes::copy_of(bytes).unwrap(), Limits::default())
}

#[test]
#[expect(clippy::too_many_lines, reason = "one check for each table")]
fn the_schema_is_the_records_layout() {
    macro_rules! check {
        ($kind:expr, $record:ty, [$($($field:ident).+),*]) => {{
            let table = schema::SCHEMA.iter().find(|table| table.kind == $kind).unwrap();
            assert_eq!(table.record, stringify!($record));
            assert_eq!(table.size, size_of::<$record>(), "{}", table.record);
            let actual: &[(&str, usize)] = &[$((
                concat!($(stringify!($field), "."),+).trim_end_matches('.'),
                std::mem::offset_of!($record, $($field).+),
            )),*];
            assert_eq!(table.fields.len(), actual.len(), "{}", table.record);
            for ((name, offset, size), (field, at)) in table.fields.iter().zip(actual) {
                assert_eq!((name, offset), (field, at), "{}", table.record);
                let next = table.fields.iter().map(|field| field.1).filter(|next| next > offset).min();
                assert_eq!(offset + size, next.unwrap_or(table.size), "{}.{name}", table.record);
            }
        }};
    }
    check!(TableKind::Files, FileRecord, [path]);
    check!(TableKind::LineAddresses, RowAddress, [address]);
    check!(
        TableKind::LineRows,
        LineRow,
        [file, line, column, flags, reserved]
    );
    check!(
        TableKind::LineExtras,
        LineExtra,
        [row, operation_index, discriminator, isa, line, column]
    );
    check!(TableKind::LineSequences, LineSequence, [first, rows]);
    check!(
        TableKind::LineRanges,
        LineRange,
        [start, end, prefix_max_end, row, statement]
    );
    check!(TableKind::StatementIndex, StatementKey, [file, line, row]);
    check!(TableKind::ControlBoundaries, ControlBoundary, [row]);
    check!(
        TableKind::Symbols,
        SymbolRecord,
        [
            name,
            address,
            end,
            extent_rank,
            storage_rank,
            kind,
            binding,
            role,
            flags
        ]
    );
    check!(TableKind::SymbolNames, NameEntry, [hash, name, value]);
    check!(
        TableKind::SymbolExtents,
        Interval,
        [start, end, prefix_max_end, value]
    );
    check!(
        TableKind::Sections,
        SectionRecord,
        [name, start, end, flags]
    );
    check!(
        TableKind::GotSlots,
        GotRecord,
        [address, target, name, kind]
    );
    check!(
        TableKind::Functions,
        FunctionRecord,
        [
            name,
            linkage_name,
            declaration.file,
            declaration.line,
            declaration.column,
            enclosing,
            coroutine,
            generics,
            generic_count,
            instances,
            instance_count,
            other_language,
            language,
            role
        ]
    );
    check!(TableKind::Generics, GenericRecord, [name, argument]);
    check!(TableKind::FunctionInstances, Member, [value]);
    check!(
        TableKind::CodeInstances,
        InstanceRecord,
        [
            function,
            parent,
            call_site.file,
            call_site.line,
            call_site.column,
            ranges,
            range_count,
            entries,
            entry_count,
            entry,
            provenance,
            inline
        ]
    );
    check!(TableKind::InstanceRanges, RangeRecord, [start, end]);
    check!(
        TableKind::RecommendedEntries,
        EntryRecord,
        [address, provenance]
    );
    check!(
        TableKind::InstructionStarts,
        StartRecord,
        [address, evidence]
    );
    check!(
        TableKind::Unwind,
        UnwindRecord,
        [
            eh_frame_base,
            text_base,
            got_base,
            eh_frame_error_offset,
            debug_frame_error_offset,
            eh_frame_error,
            debug_frame_error,
            go_table,
            go_text,
            go_func,
            go_major,
            go_minor,
            flags,
            address_size
        ]
    );
    check!(
        TableKind::GoFrameSaves,
        FrameSaveRecord,
        [start, end, saved]
    );
    check!(
        TableKind::Facts,
        FactsRecord,
        [
            embedded_reason,
            runtime_reason,
            embedded_table,
            runtime_table,
            flags
        ]
    );
    check!(
        TableKind::ThreadLocals,
        ThreadLocalRecord,
        [name, value, reason, place]
    );
    check!(TableKind::Packages, PackageRecord, [path, name]);
    check!(TableKind::PackagedNames, PackagedRecord, [package, local]);
    assert_eq!(schema::record(TableKind::FunctionNames), "NameEntry");
    assert_eq!(schema::record(TableKind::LocalNames), "NameEntry");
    assert_eq!(schema::record(TableKind::CodeRanges), "Interval");
    for kind in [
        TableKind::SymbolStorage,
        TableKind::UnsizedData,
        TableKind::SectionRanges,
        TableKind::EhFrameIndex,
        TableKind::DebugFrameIndex,
        TableKind::GoCode,
    ] {
        assert_eq!(schema::record(kind), "Interval");
    }
    let kinds = schema::SCHEMA
        .iter()
        .map(|table| table.kind)
        .collect::<Vec<_>>();
    assert_eq!(kinds, TableKind::ALL);

    // A change to any record changes this; bump the format with it.
    assert_eq!(
        schema::layout_fingerprint(),
        0xbbf0_54ff_3271_8bea,
        "the layout changed:\n{}",
        schema::schema_text()
    );
}

#[test]
fn an_image_reads_back_what_was_built() {
    let (tables, files) = sample();
    let image = seal(&tables, &files).unwrap();
    let again = reopen(image.as_bytes()).unwrap();
    assert_eq!(again.as_bytes(), image.as_bytes());

    let rows = sample_rows().into_iter().flatten().collect::<Vec<_>>();
    let decoded = (0..rows.len())
        .map(|index| {
            lines::decode_row(
                again.table::<RowAddress>(),
                again.table::<LineRow>(),
                again.table::<LineExtra>(),
                index,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(decoded, rows);
    assert_eq!(
        lines::statement_rows(
            again.table(),
            again.table(),
            again.table(),
            again.table::<LineSequence>()
        )
        .collect::<Vec<_>>(),
        tables.statement_rows()
    );
    let paths = Paths(again.bytes(TableKind::Paths));
    let read = again
        .table::<FileRecord>()
        .iter()
        .map(|file| paths.get(PathId(file.path.get())).to_path_buf())
        .collect::<Vec<_>>();
    assert_eq!(read, files.paths());
    assert_eq!(again.table::<StatementKey>().len(), 5);
    assert_eq!(again.table::<ControlBoundary>().len(), 2);
}

#[test]
fn symbols_read_back_with_their_preferences() {
    let (tables, files) = sample();
    let image = reopen(seal(&tables, &files).unwrap().as_bytes()).unwrap();
    let view = SymbolView::new(&image);
    let infos = view.all().map(symbols::Symbol::info).collect::<Vec<_>>();
    assert_eq!(infos, sample_symbols());
    assert_eq!(symbols::sections(&image), sample_sections());
    assert_eq!(symbols::got_slots(&image), sample_got());

    let id = |symbol: Option<symbols::Symbol<'_>>| symbol.map(|symbol| symbol.id().get());
    let at = ImageAddress::new;
    // A declared extent outranks an inferred one where they overlap.
    assert_eq!(id(view.code_at(at(0x1008))), Some(0));
    assert_eq!(id(view.code_at(at(0x1010))), Some(1));
    assert_eq!(id(view.code_at(at(0x1020))), None);
    assert_eq!(id(view.data_at(at(0x3007))), Some(2));
    assert_eq!(id(view.data_at(at(0x3008))), None);
    // An unsized symbol names only its own address.
    assert_eq!(id(view.data_at(at(0x3010))), Some(3));
    assert_eq!(id(view.data_at(at(0x3011))), None);
    let named = |name| {
        view.named(name)
            .map(|symbol| symbol.id().get())
            .collect::<Vec<_>>()
    };
    assert_eq!(named("counter"), [2, 3]);
    assert_eq!(named("memcpy"), [] as [u32; 0]);
    let unversioned = view.get(SymbolId::new(4)).unwrap();
    assert!(unversioned.answers_to("memcpy"));
    assert_eq!(unversioned.unversioned_name(), "memcpy");
}

#[test]
fn functions_read_back_with_their_indexes() {
    let (tables, files) = sample();
    let image = reopen(seal(&tables, &files).unwrap().as_bytes()).unwrap();
    let view = FunctionView::new(&image);
    let functions = view
        .functions()
        .map(super::functions::Function::info)
        .collect::<Vec<_>>();
    assert_eq!(functions, sample_functions());
    let instances = view
        .instances()
        .map(super::functions::CodeInstance::info)
        .collect::<Vec<_>>();
    assert_eq!(instances, sample_instances());

    let ids = |instances: &mut dyn Iterator<Item = super::functions::CodeInstance<'_>>| {
        instances
            .map(|instance| instance.id().get())
            .collect::<Vec<_>>()
    };
    let function = |id| view.function(FunctionId::new(id)).unwrap();
    assert_eq!(ids(&mut function(0).instances()), [0, 3]);
    assert_eq!(ids(&mut function(2).instances()), [] as [u32; 0]);
    assert_eq!(ids(&mut function(3).instances()), [2]);
    let named = |name| view.named(name).map(|f| f.id().get()).collect::<Vec<_>>();
    assert_eq!(named("main"), [0, 3]);
    assert_eq!(named("missing"), [] as [u32; 0]);
    // Latest start first, each instance once though it names a range twice.
    let at = ImageAddress::new;
    assert_eq!(ids(&mut view.instances_containing(at(0x1006))), [1, 0]);
    assert_eq!(
        ids(&mut view.instances_containing(at(0x1010))),
        [] as [u32; 0]
    );

    // A physical instance stops where its prologue ends; an inlined one, or
    // a coroutine's body, at its own entry; one without an entry nowhere.
    let entries = |id| {
        view.instance(CodeInstanceId::new(id))
            .unwrap()
            .recommended_entries()
            .collect::<Vec<_>>()
    };
    let entry = |address, provenance| BreakpointEntry {
        address: at(address),
        provenance,
    };
    assert_eq!(entries(0), [entry(0x1004, EntryProvenance::Statement)]);
    assert_eq!(entries(1), [entry(0x1004, EntryProvenance::RangeStart)]);
    assert_eq!(entries(2), []);
    assert_eq!(entries(3), [entry(0x4008, EntryProvenance::CoroutineBody)]);
    let starts = view
        .instruction_starts(AddressRange {
            start: at(0x1000),
            end: at(0x2000),
        })
        .count();
    assert_eq!(starts, 1);
}

#[test]
fn every_table_size_lays_out() {
    for length in [0, 1, 63, 64, 65, 4095, 4096, 4097] {
        let mut pool = vec![b'x'; length];
        if let Some(last) = pool.last_mut() {
            *last = 0;
        }
        let mut builder = Builder::new(TARGET);
        builder.bytes(TableKind::Paths, pool.clone());
        let image = builder.seal(Limits::default()).unwrap();
        assert_eq!(image.bytes(TableKind::Paths), pool, "{length}");
        assert_eq!(
            image.as_bytes().len() % format::TABLE_ALIGNMENT,
            8,
            "{length}"
        );
        reopen(image.as_bytes()).unwrap();
    }
}

#[test]
fn damage_is_caught_by_the_checksum_or_by_validation() {
    let (tables, files) = sample();
    let image = seal(&tables, &files).unwrap();
    let bytes = image.as_bytes();
    for length in 0..bytes.len() {
        assert!(reopen(&bytes[..length]).is_err(), "truncated to {length}");
    }
    let mut accepted = 0;
    for at in 0..bytes.len() {
        for flip in [0x01, 0x80, 0xff] {
            let mut damaged = bytes.to_vec();
            damaged[at] ^= flip;
            assert!(
                reopen(&damaged).is_err(),
                "byte {at} ^ {flip:#x} passed the checksum"
            );
            // Past the checksum, an image validation accepts must read
            // without a panic.
            reseal(&mut damaged);
            if let Ok(image) = reopen(&damaged) {
                read_everything(&image);
                accepted += 1;
            }
        }
    }
    // Some changes are still valid images, such as another address.
    assert!(accepted > 0);
}

#[test]
fn the_header_is_checked() {
    let (tables, files) = sample();
    let bytes = seal(&tables, &files).unwrap().as_bytes().to_vec();
    let header = |change: fn(&mut Header)| {
        let mut bytes = bytes.clone();
        let (header, _) = <Header as zerocopy::FromBytes>::mut_from_prefix(&mut bytes).unwrap();
        change(header);
        reseal(&mut bytes);
        reopen(&bytes).unwrap_err()
    };
    assert_eq!(
        header(|header| header.magic[0] = b'X'),
        ImageError::NotAnImage
    );
    assert!(matches!(
        header(|header| header.format = 2.into()),
        ImageError::Format { .. }
    ));
    assert!(matches!(
        header(|header| header.normalization = 7.into()),
        ImageError::Normalization { .. }
    ));
    assert_eq!(
        header(|header| header.layout = 1.into()),
        ImageError::Layout
    );
    assert_eq!(
        header(|header| header.length = 8.into()),
        ImageError::Truncated
    );
    assert!(matches!(
        header(|header| header.architecture = 9),
        ImageError::Malformed(_)
    ));
    assert!(matches!(
        header(|header| header.reserved[3] = 1),
        ImageError::Malformed(_)
    ));
    assert!(matches!(
        header(|header| header.tables = 99.into()),
        ImageError::Malformed(_)
    ));

    let small = Limits { bytes: 64 };
    let error = Image::from_bytes(AlignedBytes::copy_of(&bytes).unwrap(), small).unwrap_err();
    assert_eq!(error, ImageError::TooLarge);

    // The padding after each table that does not end at a boundary, and
    // before the trailer.
    let image = reopen(&bytes).unwrap();
    let mut gaps = image
        .tables
        .iter()
        .flatten()
        .map(|placed| placed.offset + placed.length)
        .filter(|end| end % format::TABLE_ALIGNMENT != 0)
        .collect::<Vec<_>>();
    assert!(!gaps.is_empty());
    gaps.push(bytes.len() - size_of::<format::Trailer>() - 1);
    for gap in gaps {
        let mut padded = bytes.clone();
        padded[gap] = 1;
        reseal(&mut padded);
        assert!(
            matches!(reopen(&padded).unwrap_err(), ImageError::Malformed(_)),
            "padding at {gap}"
        );
    }
}

/// Every table, indexes included, for sabotage.
struct Parts {
    tables: LineTables,
    ranges: Vec<LineRange>,
    statements: Vec<StatementKey>,
    boundaries: Vec<ControlBoundary>,
    files: Vec<FileRecord>,
    paths: Vec<u8>,
}

fn sabotaged(change: impl FnOnce(&mut Parts)) -> ImageError {
    let (tables, files) = sample();
    let indexes = lines::build_indexes(&tables);
    let mut paths = PathsBuilder::default();
    let records = files
        .paths()
        .iter()
        .map(|path| FileRecord {
            path: paths.intern(path).unwrap().0.into(),
        })
        .collect();
    let mut parts = Parts {
        tables,
        ranges: indexes.ranges,
        statements: indexes.statements,
        boundaries: indexes.boundaries,
        files: records,
        paths: paths.into_bytes(),
    };
    change(&mut parts);
    let mut builder = Builder::new(TARGET);
    builder
        .table(&parts.tables.addresses)
        .table(&parts.tables.rows)
        .table(&parts.tables.extras)
        .table(&parts.tables.sequences)
        .table(&parts.ranges)
        .table(&parts.statements)
        .table(&parts.boundaries)
        .table(&parts.files)
        .bytes(TableKind::Paths, parts.paths);
    builder.seal(Limits::default()).unwrap_err()
}

/// A change to the tables, and what validation says about it.
type Sabotage = (&'static str, &'static str, fn(&mut Parts));

const SABOTAGES: &[Sabotage] = &[
    (
        "an address too few",
        "rows and their addresses differ",
        |parts| {
            parts.tables.addresses.pop();
        },
    ),
    (
        "a file out of range",
        "names a file that does not exist",
        |parts| parts.tables.rows[0].file = 2.into(),
    ),
    ("an unknown flag", "flags it does not define", |parts| {
        parts.tables.rows[0].flags |= 0x80;
    }),
    ("a reserved byte", "flags it does not define", |parts| {
        parts.tables.rows[0].reserved = 1;
    }),
    ("a wide line without an extra", "has no extra", |parts| {
        parts.tables.rows[0].line = lines::WIDE_LINE.into();
    }),
    (
        "a flag without an extra",
        "rows and their extras differ",
        |parts| {
            parts.tables.rows[0].flags |= row_flags::EXTRA;
        },
    ),
    ("extras out of order", "extras are out of order", |parts| {
        parts.tables.extras.swap(0, 1);
    }),
    ("an extra for nothing", "extra it does not need", |parts| {
        parts.tables.extras[0].discriminator = 0.into();
    }),
    (
        "an extra whose line disagrees",
        "extra disagrees with its row",
        |parts| {
            parts.tables.extras[0].line = 6.into();
        },
    ),
    (
        "a narrow line in a wide extra",
        "extra disagrees with its row",
        |parts| {
            parts.tables.extras[1].line = 3.into();
        },
    ),
    ("a sequence gap", "sequences do not cover", |parts| {
        parts.tables.sequences[1].first = 7.into();
    }),
    ("a sequence past the rows", "runs past the rows", |parts| {
        parts.tables.sequences[1].rows = 4.into();
    }),
    ("an early end", "ends before its last row", |parts| {
        parts.tables.rows[1].flags |= row_flags::END_SEQUENCE;
    }),
    ("an empty range", "empty or names no located row", |parts| {
        parts.ranges[0].end = parts.ranges[0].start;
    }),
    (
        "a range at a fileless row",
        "empty or names no located row",
        |parts| {
            parts.ranges[0].row = 4.into();
        },
    ),
    (
        "ranges out of order",
        "line ranges are out of order",
        |parts| parts.ranges.swap(0, 1),
    ),
    (
        "a range's wrong running end",
        "running ends are wrong",
        |parts| {
            parts.ranges[1].prefix_max_end = 1.into();
        },
    ),
    ("a statement missing", "statement index misses", |parts| {
        parts.statements.pop();
    }),
    (
        "a statement's wrong line",
        "statement index disagrees",
        |parts| {
            // The last key, so that the index stays ordered.
            parts.statements.last_mut().unwrap().line = u64::MAX.into();
        },
    ),
    (
        "statements out of order",
        "statement index is out of order",
        |parts| parts.statements.swap(0, 1),
    ),
    (
        "a non-statement indexed",
        "statement index disagrees",
        |parts| parts.statements[0].row = 2.into(),
    ),
    ("a boundary missing", "boundary index misses", |parts| {
        parts.boundaries.pop();
    }),
    (
        "a boundary at a plain row",
        "names a row that marks none",
        |parts| parts.boundaries[0].row = 0.into(),
    ),
    (
        "boundaries out of order",
        "boundary index is out of order",
        |parts| parts.boundaries.swap(0, 1),
    ),
    ("a path past the pool", "names no path", |parts| {
        parts.files[0].path = 1000.into();
    }),
    (
        "an unterminated path",
        "path pool is not NUL-terminated",
        |parts| {
            parts.paths.pop();
        },
    ),
];

#[test]
fn validation_rejects_tables_that_disagree() {
    for (name, expected, sabotage) in SABOTAGES {
        let error = sabotaged(sabotage);
        assert!(
            matches!(&error, ImageError::Malformed(why) if why.contains(expected)),
            "{name}: {error}"
        );
    }
}

/// The sample with `kind`'s records changed and resealed, and what
/// validation says about it.
fn tampered<T>(kind: TableKind, change: impl FnOnce(&mut [T])) -> Result<(), ImageError>
where
    T: zerocopy::FromBytes + zerocopy::IntoBytes + zerocopy::KnownLayout,
{
    let (tables, files) = sample();
    let image = seal(&tables, &files).unwrap();
    let placed = image.tables[kind.index()].unwrap();
    let mut bytes = image.as_bytes().to_vec();
    change(
        <[T]>::mut_from_bytes(&mut bytes[placed.offset..placed.offset + placed.length]).unwrap(),
    );
    reseal(&mut bytes);
    reopen(&bytes).map(drop)
}

#[test]
#[expect(clippy::too_many_lines, reason = "one case for each check")]
fn validation_rejects_symbols_that_disagree() {
    use symbols::symbol_flags;
    let symbol = |change: fn(&mut [SymbolRecord])| tampered(TableKind::Symbols, change);
    let interval = |kind, change: fn(&mut [Interval])| tampered(kind, change);
    let cases = [
        (
            "an unknown kind",
            symbol(|s| s[4].kind = 9),
            "symbol is malformed",
        ),
        (
            "an unknown binding",
            symbol(|s| s[4].binding = 3),
            "symbol is malformed",
        ),
        (
            "an unknown role",
            symbol(|s| s[4].role = 99),
            "symbol is malformed",
        ),
        (
            "an unknown flag",
            symbol(|s| s[4].flags |= 0x80),
            "symbol is malformed",
        ),
        (
            "a name past the pool",
            symbol(|s| s[4].name = 9999.into()),
            "symbol is malformed",
        ),
        (
            "data with an extent",
            symbol(|s| s[0].kind = 2),
            "symbol is malformed",
        ),
        (
            "an extent and storage",
            symbol(|s| s[0].flags |= symbol_flags::STORAGE),
            "symbol is malformed",
        ),
        (
            "an inferred storage",
            symbol(|s| s[2].flags |= symbol_flags::INFERRED),
            "symbol is malformed",
        ),
        (
            "an empty extent",
            symbol(|s| s[0].end = s[0].address),
            "symbol is malformed",
        ),
        (
            "a repeated rank",
            symbol(|s| s[1].extent_rank = s[0].extent_rank),
            "out of range or repeated",
        ),
        (
            "a rank out of range",
            symbol(|s| s[2].storage_rank = 2.into()),
            "out of range or repeated",
        ),
        (
            "a rank it cannot have",
            symbol(|s| s[4].extent_rank = 0.into()),
            "a rank it cannot",
        ),
        (
            "an extent unindexed",
            interval(TableKind::SymbolExtents, |i| i[1].value = 0.into()),
            "indexes disagree",
        ),
        (
            "an index entry's wrong end",
            interval(TableKind::SymbolStorage, |i| {
                i[0].end = (i[0].end.get() - 1).into();
            }),
            "indexes disagree",
        ),
        (
            "unsized data's wrong end",
            interval(TableKind::UnsizedData, |i| {
                i[0].end = (i[0].end.get() + 1).into();
                i[0].prefix_max_end = i[0].end;
            }),
            "indexes disagree",
        ),
        (
            "a name for another symbol",
            tampered(TableKind::SymbolNames, |n: &mut [NameEntry]| {
                n[0].value = 4.into();
            }),
            "name index disagrees",
        ),
        (
            "names out of order",
            tampered(TableKind::SymbolNames, |n: &mut [NameEntry]| n.swap(0, 1)),
            "name index disagrees",
        ),
        (
            "an empty section",
            tampered(TableKind::Sections, |s: &mut [SectionRecord]| {
                s[0].end = s[0].start;
            }),
            "section is malformed",
        ),
        (
            "a section unindexed",
            interval(TableKind::SectionRanges, |i| i[1].value = 0.into()),
            "section index disagrees",
        ),
        (
            "an unknown GOT slot",
            tampered(TableKind::GotSlots, |g: &mut [GotRecord]| g[0].kind = 2),
            "GOT slot is malformed",
        ),
        (
            "a named indirect slot",
            tampered(TableKind::GotSlots, |g: &mut [GotRecord]| {
                g[1].name = g[0].name;
            }),
            "GOT slot is malformed",
        ),
    ];
    for (name, error, expected) in cases {
        assert!(
            matches!(&error, Err(ImageError::Malformed(why)) if why.contains(expected)),
            "{name}: {error:?}"
        );
    }
}

#[test]
#[expect(clippy::too_many_lines, reason = "one case for each check")]
fn validation_rejects_functions_that_disagree() {
    let function = |change: fn(&mut [FunctionRecord])| tampered(TableKind::Functions, change);
    let instance = |change: fn(&mut [InstanceRecord])| tampered(TableKind::CodeInstances, change);
    let cases = [
        (
            "a name past the pool",
            function(|f| f[0].name = 9999.into()),
            "function is malformed",
        ),
        (
            "an unknown language",
            function(|f| f[0].language = 9),
            "function is malformed",
        ),
        (
            "another language's code on a known one",
            function(|f| f[0].other_language = 1.into()),
            "function is malformed",
        ),
        (
            "an unknown role",
            function(|f| f[0].role = 99),
            "function is malformed",
        ),
        (
            "a missing enclosure",
            function(|f| f[1].enclosing = 9.into()),
            "function is malformed",
        ),
        (
            "a declaration on line zero",
            function(|f| f[0].declaration.line = 0.into()),
            "function is malformed",
        ),
        (
            "a declaration in a missing file",
            function(|f| f[0].declaration.file = 7.into()),
            "function is malformed",
        ),
        (
            "generics past their table",
            function(|f| f[0].generic_count = 9.into()),
            "function is malformed",
        ),
        (
            "an instance claimed twice",
            function(|f| f[2].instance_count = 1.into()),
            "instance index disagrees",
        ),
        (
            "another function's instance",
            tampered(TableKind::FunctionInstances, |m: &mut [Member]| {
                m.swap(1, 2);
            }),
            "instance index disagrees",
        ),
        (
            "a name for another function",
            tampered(TableKind::FunctionNames, |n: &mut [NameEntry]| {
                n[0].value = 0.into();
            }),
            "name index disagrees",
        ),
        (
            "an unknown kind",
            instance(|i| i[0].inline = 2),
            "instance is malformed",
        ),
        (
            "a later parent",
            instance(|i| i[1].parent = 1.into()),
            "instance is malformed",
        ),
        (
            "a physical instance's call site",
            instance(|i| i[0].call_site = i[1].call_site),
            "instance is malformed",
        ),
        (
            "ranges past their table",
            instance(|i| i[3].range_count = 9.into()),
            "instance is malformed",
        ),
        (
            "an unknown provenance",
            instance(|i| i[0].provenance = 9),
            "instance is malformed",
        ),
        (
            "an entry without a provenance",
            instance(|i| i[2].entry = 1.into()),
            "instance is malformed",
        ),
        (
            "an empty range",
            tampered(TableKind::InstanceRanges, |r: &mut [RangeRecord]| {
                r[0].end = r[0].start;
            }),
            "ranges or entries are malformed",
        ),
        (
            "a range unindexed",
            tampered(TableKind::CodeRanges, |i: &mut [Interval]| {
                i[0].value = 3.into();
            }),
            "code range index disagrees",
        ),
        (
            "unordered starts",
            tampered(TableKind::InstructionStarts, |s: &mut [StartRecord]| {
                s.swap(0, 1);
            }),
            "instruction starts are malformed",
        ),
        (
            "an unknown evidence",
            tampered(TableKind::InstructionStarts, |s: &mut [StartRecord]| {
                s[0].evidence = 9;
            }),
            "instruction starts are malformed",
        ),
    ];
    for (name, error, expected) in cases {
        assert!(
            matches!(&error, Err(ImageError::Malformed(why)) if why.contains(expected)),
            "{name}: {error:?}"
        );
    }
}

#[test]
fn unwinding_tables_read_back_as_their_lookups() {
    let (tables, files) = sample();
    let image = reopen(seal(&tables, &files).unwrap().as_bytes()).unwrap();
    let view = UnwindView::new(&image);
    let sample = sample_unwind();
    assert_eq!(view.eh_frame(), &*sample.eh_frame);
    assert_eq!(view.debug_frame(), &*sample.debug_frame);
    assert_eq!(
        (view.eh_frame_base(), view.text_base(), view.got_base()),
        (Some(0x9000), Some(0x1000), None)
    );
    assert_eq!((view.big_endian(), view.address_size()), (false, 8));

    // The earliest entry in the section wins, unless the section is
    // malformed before it.
    let eh_frame = view.eh_frame_index();
    let debug_frame = view.debug_frame_index();
    for (address, before, kept) in [
        (0x1090, Ok(8), eh_frame),
        (0x1150, Ok(24), eh_frame),
        (
            0x1450,
            Err(FdeMiss::Malformed("a CIE is malformed")),
            eh_frame,
        ),
        (
            0x0fff,
            Err(FdeMiss::Malformed("a CIE is malformed")),
            eh_frame,
        ),
        (0x2050, Ok(0), debug_frame),
        (0x2100, Err(FdeMiss::NoEntry), debug_frame),
    ] {
        assert_eq!(kept.lookup(address), before, "{address:#x}");
    }
    assert_eq!(eh_frame.entries, sample.eh_frame_index.entries);
    for (address, go) in [
        (0x2fff, false),
        (0x3000, true),
        (0x3100, false),
        (0x32ff, true),
    ] {
        assert_eq!(
            view.is_go_code(ImageAddress::new(address)),
            go,
            "{address:#x}"
        );
    }
    let go = sample.go.unwrap();
    let (bytes, facts) = view.go().unwrap();
    assert_eq!((bytes, facts), (&*go.bytes, go.facts));
    assert_eq!(view.frame_saves().collect::<Vec<_>>(), go.frame_saves);
}

#[test]
fn validation_rejects_unwinding_tables_that_disagree() {
    let facts = |change: fn(&mut [UnwindRecord])| tampered(TableKind::Unwind, change);
    let saves = |change: fn(&mut [FrameSaveRecord])| tampered(TableKind::GoFrameSaves, change);
    let interval = |kind, change: fn(&mut [Interval])| tampered(kind, change);
    let cases = [
        (
            "an unknown flag",
            facts(|u| u[0].flags |= 0x80),
            "facts are malformed",
        ),
        (
            "an address size of 2",
            facts(|u| u[0].address_size = 2),
            "facts are malformed",
        ),
        (
            "an error past its section",
            facts(|u| u[0].eh_frame_error_offset = 64.into()),
            "facts are malformed",
        ),
        (
            "an error offset without an error",
            facts(|u| u[0].debug_frame_error_offset = 4.into()),
            "facts are malformed",
        ),
        (
            "an error past the pool",
            facts(|u| u[0].eh_frame_error = 9999.into()),
            "facts are malformed",
        ),
        (
            "a base without its flag",
            facts(|u| u[0].got_base = 0x10.into()),
            "facts are malformed",
        ),
        (
            "Go facts without Go's table",
            facts(|u| u[0].flags &= !unwind_flags::GO_TABLE),
            "facts are malformed",
        ),
        (
            "a release without its flag",
            facts(|u| u[0].flags &= !unwind_flags::GO_RELEASE),
            "facts are malformed",
        ),
        (
            "an entry past its section",
            interval(TableKind::EhFrameIndex, |i| i[0].value = 64.into()),
            "unwinding index is malformed",
        ),
        (
            "unordered entries",
            interval(TableKind::DebugFrameIndex, |i| i[0].start = 0x2100.into()),
            "unwinding index is malformed",
        ),
        (
            "a Go range's wrong reach",
            interval(TableKind::GoCode, |i| i[1].prefix_max_end = 0x3100.into()),
            "unwinding index is malformed",
        ),
        (
            "an unknown save",
            saves(|s| s[0].saved = 2),
            "frame save is malformed",
        ),
        (
            "a backward save",
            saves(|s| s[0].end = 0x3000.into()),
            "frame save is malformed",
        ),
        (
            "a range never saved",
            saves(|s| s[1].end = 4.into()),
            "frame save is malformed",
        ),
    ];
    for (name, error, expected) in cases {
        assert!(
            matches!(&error, Err(ImageError::Malformed(why)) if why.contains(expected)),
            "{name}: {error:?}"
        );
    }
}

#[test]
fn packages_read_back_with_functions_by_local_name() {
    let (tables, files) = sample();
    let image = reopen(seal(&tables, &files).unwrap().as_bytes()).unwrap();
    let view = PackageView::new(&image);
    assert_eq!(view.package_name("main"), Some("main"));
    assert_eq!(view.package_name("example.com/m/stack"), Some("stack"));
    assert_eq!(view.package_name("example.com/m"), None);
    for (index, expected) in sample_packaged().iter().enumerate() {
        let function = FunctionId::new(u32::try_from(index).unwrap());
        assert_eq!(
            view.packaged_name(function),
            expected
                .as_ref()
                .map(|(package, local)| (*package, local.as_str())),
        );
    }
    assert_eq!(view.packaged_name(FunctionId::new(4)), None);
    assert_eq!(
        view.with_local_name("Push").collect::<Vec<_>>(),
        [FunctionId::new(1), FunctionId::new(3)]
    );
    assert_eq!(view.with_local_name("Pop").count(), 0);
    assert_eq!(SAMPLE_PACKAGES.len(), 3);
}

#[test]
fn validation_rejects_packages_that_disagree() {
    let package = |change: fn(&mut [PackageRecord])| tampered(TableKind::Packages, change);
    let packaged = |change: fn(&mut [PackagedRecord])| tampered(TableKind::PackagedNames, change);
    let local = |change: fn(&mut [NameEntry])| tampered(TableKind::LocalNames, change);
    let cases = [
        (
            "unordered packages",
            package(|p| p.swap(0, 1)),
            "package is malformed",
        ),
        (
            "a path past the pool",
            package(|p| p[0].path = 9999.into()),
            "package is malformed",
        ),
        (
            "a local without a package",
            packaged(|p| p[1].package = NONE.into()),
            "packaged name",
        ),
        (
            "a package without a local",
            packaged(|p| p[0].package = p[1].package),
            "packaged name",
        ),
        (
            "an unindexed local",
            packaged(|p| p[0] = p[1]),
            "local name index",
        ),
        (
            "another function's name",
            local(|n| n[0].value = 0.into()),
            "local name index",
        ),
        (
            "a function named twice",
            local(|n| n[1].value = n[0].value),
            "local name index",
        ),
    ];
    for (name, error, expected) in cases {
        assert!(
            matches!(&error, Err(ImageError::Malformed(why)) if why.contains(expected)),
            "{name}: {error:?}"
        );
    }
}

#[test]
fn facts_read_back_with_thread_locals_by_name() {
    let (tables, files) = sample();
    let image = reopen(seal(&tables, &files).unwrap().as_bytes()).unwrap();
    let view = FactsView::new(&image);
    assert_eq!(view.symbol_sources(), sample_sources());
    assert!(view.thread_local_storage());
    let expected = sample_thread_locals();
    let read = view
        .thread_locals()
        .map(|(name, place)| (std::sync::Arc::from(name), place))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(read, expected);
    for (name, place) in &expected {
        assert_eq!(view.thread_local(name).as_ref(), Some(place), "{name}");
    }
    assert_eq!(view.thread_local("count"), None);
    assert_eq!(view.thread_local("zzz"), None);
}

#[test]
fn validation_rejects_facts_that_disagree() {
    let facts = |change: fn(&mut [FactsRecord])| tampered(TableKind::Facts, change);
    let local = |change: fn(&mut [ThreadLocalRecord])| tampered(TableKind::ThreadLocals, change);
    let cases = [
        (
            "an unknown flag",
            facts(|f| f[0].flags |= 0x80),
            "facts are malformed",
        ),
        (
            "an unknown state",
            facts(|f| f[0].runtime_table = 3),
            "facts are malformed",
        ),
        (
            "a reason for a loaded table",
            facts(|f| f[0].runtime_reason = f[0].embedded_reason),
            "facts are malformed",
        ),
        (
            "an unusable table without a reason",
            facts(|f| f[0].embedded_reason = NONE.into()),
            "facts are malformed",
        ),
        (
            "an unknown place",
            local(|l| l[0].place = 3),
            "thread-local variable",
        ),
        (
            "unordered names",
            local(|l| l.swap(0, 1)),
            "thread-local variable",
        ),
        (
            "a repeated name",
            local(|l| l[1].name = l[0].name),
            "thread-local variable",
        ),
        (
            "a known place's reason",
            local(|l| l[0].reason = l[2].reason),
            "thread-local variable",
        ),
        (
            "an unknown place's value",
            local(|l| l[2].value = 1.into()),
            "thread-local variable",
        ),
        (
            "a name past the pool",
            local(|l| l[0].name = 9999.into()),
            "thread-local variable",
        ),
    ];
    for (name, error, expected) in cases {
        assert!(
            matches!(&error, Err(ImageError::Malformed(why)) if why.contains(expected)),
            "{name}: {error:?}"
        );
    }
}

mod snapshot {
    use std::io::Write as _;

    use super::super::AlignedBytes;
    use super::super::backing::{FileStamp, ReadError};

    fn read_snapshot(
        path: &std::path::Path,
        limit: u64,
    ) -> Result<(AlignedBytes, FileStamp), ReadError> {
        read_snapshot_with(path, limit, &mut |_| {})
    }

    fn read_snapshot_with(
        path: &std::path::Path,
        limit: u64,
        between: &mut dyn FnMut(u32),
    ) -> Result<(AlignedBytes, FileStamp), ReadError> {
        super::super::backing::read_snapshot(path, limit, between, AlignedBytes::zeroed)
    }

    /// A directory removed when the test ends.
    struct ScratchDir(std::path::PathBuf);

    impl ScratchDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("uscope-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_snapshot_is_the_whole_file() {
        let dir = ScratchDir::new("image-snapshot");
        for length in [0_u16, 1, 4095, 4096, 4097] {
            let path = dir.path().join(format!("{length}"));
            let contents = (0..length)
                .map(|at| at.to_le_bytes()[0])
                .collect::<Vec<_>>();
            std::fs::write(&path, &contents).unwrap();
            let (bytes, stamp) = read_snapshot(&path, 1 << 20).unwrap();
            assert_eq!(&*bytes, contents, "{length}");
            assert_eq!(stamp.length, u64::from(length));
        }
        assert!(matches!(
            read_snapshot(&dir.path().join("4097"), 4096),
            Err(ReadError::TooLarge { .. })
        ));
        assert!(matches!(
            read_snapshot(dir.path(), 1),
            Err(ReadError::NotAFile { .. })
        ));
    }

    /// A stream is read to its end, within the limit.
    #[test]
    fn a_fifo_is_read_as_it_is_written() {
        let dir = ScratchDir::new("image-fifo");
        let path = dir.path().join("fifo");
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).unwrap();
        for (contents, limit) in [(vec![7_u8; 5000], 1 << 20), (vec![1_u8; 65], 64)] {
            let writer = {
                let (path, contents) = (path.clone(), contents.clone());
                std::thread::spawn(move || {
                    // A reader that stops at its limit closes the FIFO
                    // under the writer.
                    let _ = std::fs::write(path, contents);
                })
            };
            let read = read_snapshot(&path, limit);
            writer.join().unwrap();
            if contents.len() as u64 <= limit {
                let (bytes, stamp) = read.unwrap();
                assert_eq!(&*bytes, contents);
                assert_eq!(stamp.length, contents.len() as u64);
            } else {
                assert!(matches!(read, Err(ReadError::TooLarge { .. })));
            }
        }
    }

    #[test]
    fn a_file_that_changes_is_read_again() {
        let dir = ScratchDir::new("image-changing");
        let path = dir.path().join("changing");
        std::fs::write(&path, b"first").unwrap();
        let (bytes, _) = read_snapshot_with(&path, 1 << 20, &mut |attempt| {
            if attempt == 0 {
                std::fs::write(&path, b"second, longer").unwrap();
            }
        })
        .unwrap();
        assert_eq!(&*bytes, b"second, longer");

        // A file that grows under every read never yields a snapshot.
        let error = read_snapshot_with(&path, 1 << 20, &mut |_| {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(b"!").unwrap();
        })
        .unwrap_err();
        assert!(
            matches!(error, ReadError::Changed { attempts: 3, .. }),
            "{error}"
        );
    }
}
