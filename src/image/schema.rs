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
    TableSchema {
        kind: TableKind::Symbols,
        record: "SymbolRecord",
        size: 32,
        fields: &[
            ("name", 0, 4),
            ("address", 4, 8),
            ("end", 12, 8),
            ("extent_rank", 20, 4),
            ("storage_rank", 24, 4),
            ("kind", 28, 1),
            ("binding", 29, 1),
            ("role", 30, 1),
            ("flags", 31, 1),
        ],
    },
    names(TableKind::SymbolNames),
    intervals(TableKind::SymbolExtents),
    intervals(TableKind::SymbolStorage),
    intervals(TableKind::UnsizedData),
    TableSchema {
        kind: TableKind::Sections,
        record: "SectionRecord",
        size: 21,
        fields: &[
            ("name", 0, 4),
            ("start", 4, 8),
            ("end", 12, 8),
            ("flags", 20, 1),
        ],
    },
    intervals(TableKind::SectionRanges),
    TableSchema {
        kind: TableKind::GotSlots,
        record: "GotRecord",
        size: 21,
        fields: &[
            ("address", 0, 8),
            ("target", 8, 8),
            ("name", 16, 4),
            ("kind", 20, 1),
        ],
    },
    TableSchema {
        kind: TableKind::Functions,
        record: "FunctionRecord",
        size: 56,
        fields: &[
            ("name", 0, 4),
            ("linkage_name", 4, 4),
            ("declaration.file", 8, 4),
            ("declaration.line", 12, 8),
            ("declaration.column", 20, 8),
            ("enclosing", 28, 4),
            ("coroutine", 32, 4),
            ("generics", 36, 4),
            ("generic_count", 40, 4),
            ("instances", 44, 4),
            ("instance_count", 48, 4),
            ("other_language", 52, 2),
            ("language", 54, 1),
            ("role", 55, 1),
        ],
    },
    names(TableKind::FunctionNames),
    TableSchema {
        kind: TableKind::Generics,
        record: "GenericRecord",
        size: 8,
        fields: &[("name", 0, 4), ("argument", 4, 4)],
    },
    TableSchema {
        kind: TableKind::FunctionInstances,
        record: "Member",
        size: 4,
        fields: &[("value", 0, 4)],
    },
    TableSchema {
        kind: TableKind::CodeInstances,
        record: "InstanceRecord",
        size: 54,
        fields: &[
            ("function", 0, 4),
            ("parent", 4, 4),
            ("call_site.file", 8, 4),
            ("call_site.line", 12, 8),
            ("call_site.column", 20, 8),
            ("ranges", 28, 4),
            ("range_count", 32, 4),
            ("entries", 36, 4),
            ("entry_count", 40, 4),
            ("entry", 44, 8),
            ("provenance", 52, 1),
            ("inline", 53, 1),
        ],
    },
    TableSchema {
        kind: TableKind::InstanceRanges,
        record: "RangeRecord",
        size: 16,
        fields: &[("start", 0, 8), ("end", 8, 8)],
    },
    intervals(TableKind::CodeRanges),
    TableSchema {
        kind: TableKind::RecommendedEntries,
        record: "EntryRecord",
        size: 9,
        fields: &[("address", 0, 8), ("provenance", 8, 1)],
    },
    TableSchema {
        kind: TableKind::InstructionStarts,
        record: "StartRecord",
        size: 9,
        fields: &[("address", 0, 8), ("evidence", 8, 1)],
    },
    TableSchema {
        kind: TableKind::Unwind,
        record: "UnwindRecord",
        size: 82,
        fields: &[
            ("eh_frame_base", 0, 8),
            ("text_base", 8, 8),
            ("got_base", 16, 8),
            ("eh_frame_error_offset", 24, 8),
            ("debug_frame_error_offset", 32, 8),
            ("eh_frame_error", 40, 4),
            ("debug_frame_error", 44, 4),
            ("go_table", 48, 8),
            ("go_text", 56, 8),
            ("go_func", 64, 8),
            ("go_major", 72, 4),
            ("go_minor", 76, 4),
            ("flags", 80, 1),
            ("address_size", 81, 1),
        ],
    },
    bytes(TableKind::EhFrame),
    bytes(TableKind::DebugFrame),
    intervals(TableKind::EhFrameIndex),
    intervals(TableKind::DebugFrameIndex),
    intervals(TableKind::GoCode),
    bytes(TableKind::GoTable),
    TableSchema {
        kind: TableKind::GoFrameSaves,
        record: "FrameSaveRecord",
        size: 17,
        fields: &[("start", 0, 8), ("end", 8, 8), ("saved", 16, 1)],
    },
    TableSchema {
        kind: TableKind::Facts,
        record: "FactsRecord",
        size: 11,
        fields: &[
            ("embedded_reason", 0, 4),
            ("runtime_reason", 4, 4),
            ("embedded_table", 8, 1),
            ("runtime_table", 9, 1),
            ("flags", 10, 1),
        ],
    },
    TableSchema {
        kind: TableKind::ThreadLocals,
        record: "ThreadLocalRecord",
        size: 17,
        fields: &[
            ("name", 0, 4),
            ("value", 4, 8),
            ("reason", 12, 4),
            ("place", 16, 1),
        ],
    },
    TableSchema {
        kind: TableKind::Packages,
        record: "PackageRecord",
        size: 8,
        fields: &[("path", 0, 4), ("name", 4, 4)],
    },
    TableSchema {
        kind: TableKind::PackagedNames,
        record: "PackagedRecord",
        size: 8,
        fields: &[("package", 0, 4), ("local", 4, 4)],
    },
    names(TableKind::LocalNames),
];

const fn bytes(kind: TableKind) -> TableSchema {
    TableSchema {
        kind,
        record: "u8",
        size: 1,
        fields: &[],
    }
}

const fn intervals(kind: TableKind) -> TableSchema {
    TableSchema {
        kind,
        record: "Interval",
        size: 28,
        fields: &[
            ("start", 0, 8),
            ("end", 8, 8),
            ("prefix_max_end", 16, 8),
            ("value", 24, 4),
        ],
    }
}

const fn names(kind: TableKind) -> TableSchema {
    TableSchema {
        kind,
        record: "NameEntry",
        size: 12,
        fields: &[("hash", 0, 4), ("name", 4, 4), ("value", 8, 4)],
    }
}

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

/// The name of the record a table holds.
pub(super) fn record(kind: TableKind) -> &'static str {
    SCHEMA
        .iter()
        .find(|table| table.kind == kind)
        .map_or("", |table| table.record)
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
