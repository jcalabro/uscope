//! A small image with every kind of row, which tests and the fuzz harness
//! change to see what validation makes of it.

use std::path::PathBuf;

use zerocopy::IntoBytes as _;

use super::format::Trailer;
use super::lines::{
    self, FileRecord, LineExtra, LineRange, LineRow, LineSequence, LineTables, Row, RowAddress,
};
use super::{Builder, Image, ImageError, Limits, PathId, Paths, TableKind};
use crate::{AddressRange, ImageAddress, SourceFileId};

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

pub(super) fn seal(tables: &LineTables, files: &lines::Files) -> Result<Image, ImageError> {
    let mut builder = Builder::new(TARGET);
    tables.add_to(&mut builder);
    files.add_to(&mut builder).unwrap();
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
