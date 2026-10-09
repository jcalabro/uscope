//! What every table's records look like, and the fingerprint of it all
//! that each image's header carries: an image whose fingerprint differs
//! was written by a build whose records differ, and is not read.

use std::sync::OnceLock;

use super::format::{self, Header, TableKind};
use super::lines;
use super::validate::ImageError;
use crate::{Architecture, ByteOrder, PointerWidth, TargetDescription};

/// One field of a record: its name, offset, and size.
pub(super) type Field = (&'static str, usize, usize);

/// One table: its kind, its record's name and size, and its fields.
pub(super) struct TableSchema {
    pub kind: TableKind,
    pub record: &'static str,
    pub size: usize,
    pub fields: &'static [Field],
}

/// Every table's layout. A test checks each field against the record's
/// actual offsets and sizes, so this cannot drift from the code.
pub(super) const SCHEMA: &[TableSchema] = &[
    TableSchema {
        kind: TableKind::Strings,
        record: "u8",
        size: 1,
        fields: &[],
    },
    TableSchema {
        kind: TableKind::Files,
        record: "FileRecord",
        size: 4,
        fields: &[("path", 0, 4)],
    },
    TableSchema {
        kind: TableKind::LineAddresses,
        record: "RowAddress",
        size: 8,
        fields: &[("address", 0, 8)],
    },
    TableSchema {
        kind: TableKind::LineRows,
        record: "LineRow",
        size: 12,
        fields: &[
            ("file", 0, 4),
            ("line", 4, 4),
            ("column", 8, 2),
            ("flags", 10, 1),
            ("reserved", 11, 1),
        ],
    },
    TableSchema {
        kind: TableKind::LineExtras,
        record: "LineExtra",
        size: 44,
        fields: &[
            ("row", 0, 4),
            ("operation_index", 4, 8),
            ("discriminator", 12, 8),
            ("isa", 20, 8),
            ("line", 28, 8),
            ("column", 36, 8),
        ],
    },
    TableSchema {
        kind: TableKind::LineSequences,
        record: "LineSequence",
        size: 8,
        fields: &[("first", 0, 4), ("rows", 4, 4)],
    },
    TableSchema {
        kind: TableKind::LineRanges,
        record: "LineRange",
        size: 29,
        fields: &[
            ("start", 0, 8),
            ("end", 8, 8),
            ("prefix_max_end", 16, 8),
            ("row", 24, 4),
            ("statement", 28, 1),
        ],
    },
    TableSchema {
        kind: TableKind::StatementIndex,
        record: "StatementKey",
        size: 16,
        fields: &[("file", 0, 4), ("line", 4, 8), ("row", 12, 4)],
    },
    TableSchema {
        kind: TableKind::ControlBoundaries,
        record: "ControlBoundary",
        size: 4,
        fields: &[("row", 0, 4)],
    },
    TableSchema {
        kind: TableKind::Paths,
        record: "u8",
        size: 1,
        fields: &[],
    },
];

/// The layout fingerprint: XXH3-64 of the schema's canonical text, with
/// the container's version.
pub(super) fn layout_fingerprint() -> u64 {
    static FINGERPRINT: OnceLock<u64> = OnceLock::new();
    *FINGERPRINT.get_or_init(|| twox_hash::XxHash3_64::oneshot(schema_text().as_bytes()))
}

/// The schema as one line per table.
pub(super) fn schema_text() -> String {
    use std::fmt::Write as _;
    let mut text = format!("format {}\n", format::FORMAT_VERSION);
    for table in SCHEMA {
        let _ = write!(
            text,
            "{} {} {} {}",
            table.kind as u32,
            table.record,
            table.size,
            table.fields.len()
        );
        for (name, offset, size) in table.fields {
            let _ = write!(text, " {name}@{offset}:{size}");
        }
        text.push('\n');
    }
    text
}

/// The stride the schema gives a table.
pub(super) fn stride(kind: TableKind) -> usize {
    SCHEMA
        .iter()
        .find(|table| table.kind == kind)
        .map_or(0, |table| table.size)
}

const _: () = assert!(size_of::<lines::LineExtra>() == 44);
const _: () = assert!(size_of::<lines::StatementKey>() == 16);

/// A new image's header, but for its magic, which sealing writes last.
pub(super) fn header(
    target: TargetDescription,
    length: usize,
    tables: usize,
) -> Result<Header, ImageError> {
    Ok(Header {
        magic: [0; 8],
        format: format::FORMAT_VERSION.into(),
        normalization: format::NORMALIZATION_REVISION.into(),
        layout: layout_fingerprint().into(),
        length: (length as u64).into(),
        tables: u32::try_from(tables)
            .map_err(|_| ImageError::TooLarge)?
            .into(),
        architecture: match target.architecture {
            Architecture::X86_64 => 1,
            Architecture::Aarch64 => 2,
        },
        byte_order: match target.byte_order {
            ByteOrder::Little => 1,
            ByteOrder::Big => 2,
        },
        address_size: target.pointer_width.bytes(),
        flags: 0,
        reserved: [0; 24],
    })
}

/// The target a header describes, or `None` for one it cannot.
pub(super) const fn target_of(header: &Header) -> Option<TargetDescription> {
    let architecture = match header.architecture {
        1 => Architecture::X86_64,
        2 => Architecture::Aarch64,
        _ => return None,
    };
    let byte_order = match header.byte_order {
        1 => ByteOrder::Little,
        2 => ByteOrder::Big,
        _ => return None,
    };
    let pointer_width = match header.address_size {
        4 => PointerWidth::Bits32,
        8 => PointerWidth::Bits64,
        _ => return None,
    };
    Some(TargetDescription {
        architecture,
        byte_order,
        pointer_width,
    })
}
