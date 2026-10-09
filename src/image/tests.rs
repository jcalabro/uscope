use super::format::Header;
use super::lines::{
    self, ControlBoundary, FileRecord, LineExtra, LineRange, LineRow, LineSequence, LineTables,
    RowAddress, StatementKey, row_flags,
};
use super::sample::{TARGET, read_everything, reseal, sample, sample_rows, seal};
use super::*;

fn reopen(bytes: &[u8]) -> Result<Image, ImageError> {
    Image::from_bytes(AlignedBytes::copy_of(bytes).unwrap(), Limits::default())
}

#[test]
fn the_schema_is_the_records_layout() {
    macro_rules! check {
        ($kind:expr, $record:ty, [$($field:ident),*]) => {{
            let table = schema::SCHEMA.iter().find(|table| table.kind == $kind).unwrap();
            assert_eq!(table.record, stringify!($record));
            assert_eq!(table.size, size_of::<$record>(), "{}", table.record);
            let actual: &[(&str, usize)] = &[$((stringify!($field), std::mem::offset_of!($record, $field))),*];
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
    let kinds = schema::SCHEMA
        .iter()
        .map(|table| table.kind)
        .collect::<Vec<_>>();
    assert_eq!(kinds, TableKind::ALL);

    // A change to any record changes this; bump the format with it.
    assert_eq!(
        schema::layout_fingerprint(),
        0x53a7_7ece_73f1_1bf4,
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
