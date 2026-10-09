//! Checks an image completely before anything reads it: its header and
//! checksum, the directory's tables and the zeroed padding between them,
//! and every reference, flag, and index in the tables. Each check is one
//! pass over a table, and no check recurses.

use zerocopy::FromBytes as _;

use super::format::{self, DirectoryEntry, Header, TableKind, Trailer};
use super::lines::{
    self, ControlBoundary, FileRecord, LineExtra, LineRange, LineRow, LineSequence, RowAddress,
    StatementKey, row_flags,
};
use super::{Image, MAX_ROWS, NONE, Placed, schema, strings};

/// Why bytes are not a usable image.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImageError {
    #[error("the image is larger than an image may be")]
    TooLarge,
    #[error("the image is truncated")]
    Truncated,
    #[error("the bytes are not an image")]
    NotAnImage,
    #[error("the image's format version is {found}, not {expected}")]
    Format { found: u32, expected: u32 },
    #[error("the image was normalized by revision {found}, not {expected}")]
    Normalization { found: u32, expected: u32 },
    #[error("the image's records are laid out differently")]
    Layout,
    #[error("the image's checksum does not match its contents")]
    Checksum,
    #[error("the image is malformed: {0}")]
    Malformed(String),
}

/// Checks one family of tables.
type Validation = fn(&Image) -> Result<(), String>;

fn malformed(text: impl Into<String>) -> ImageError {
    ImageError::Malformed(text.into())
}

/// The most an image may hold.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Its length in bytes.
    pub bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self { bytes: 4 << 30 }
    }
}

/// Checks the header, trailer, and directory, returning where each table
/// is.
pub(super) fn structure(
    bytes: &[u8],
    limits: Limits,
) -> Result<[Option<Placed>; TableKind::COUNT], ImageError> {
    let (header, _) = Header::ref_from_prefix(bytes).map_err(|_| ImageError::Truncated)?;
    if header.magic != format::MAGIC {
        return Err(ImageError::NotAnImage);
    }
    if header.format.get() != format::FORMAT_VERSION {
        return Err(ImageError::Format {
            found: header.format.get(),
            expected: format::FORMAT_VERSION,
        });
    }
    if header.normalization.get() != format::NORMALIZATION_REVISION {
        return Err(ImageError::Normalization {
            found: header.normalization.get(),
            expected: format::NORMALIZATION_REVISION,
        });
    }
    if header.layout.get() != schema::layout_fingerprint() {
        return Err(ImageError::Layout);
    }
    let length = header.length.get();
    if length > limits.bytes {
        return Err(ImageError::TooLarge);
    }
    if length != bytes.len() as u64 {
        return Err(ImageError::Truncated);
    }
    if schema::target_of(header).is_none() || header.flags != 0 || header.reserved != [0; 24] {
        return Err(malformed("the header has fields it does not define"));
    }
    let trailer_start = bytes
        .len()
        .checked_sub(size_of::<Trailer>())
        .filter(|start| *start >= size_of::<Header>())
        .ok_or(ImageError::Truncated)?;
    let trailer =
        Trailer::ref_from_bytes(&bytes[trailer_start..]).map_err(|_| ImageError::Truncated)?;
    if trailer.checksum.get() != twox_hash::XxHash3_64::oneshot(&bytes[..trailer_start]) {
        return Err(ImageError::Checksum);
    }

    let count = header.tables.get() as usize;
    if count > TableKind::COUNT {
        return Err(malformed(
            "the directory lists more tables than there are kinds",
        ));
    }
    let directory_end = size_of::<Header>() + count * size_of::<DirectoryEntry>();
    if directory_end > trailer_start {
        return Err(ImageError::Truncated);
    }
    let entries = <[DirectoryEntry]>::ref_from_bytes(&bytes[size_of::<Header>()..directory_end])
        .map_err(|_| ImageError::Truncated)?;
    let mut tables = [None; TableKind::COUNT];
    let mut previous_kind = None;
    let mut previous_end = directory_end;
    for entry in entries {
        let kind = TableKind::from_number(entry.kind.get())
            .ok_or_else(|| malformed(format!("unknown table kind {}", entry.kind.get())))?;
        if previous_kind.is_some_and(|previous| previous >= kind) {
            return Err(malformed("tables are out of order or repeated"));
        }
        previous_kind = Some(kind);
        let stride = entry.stride.get() as usize;
        if stride != schema::stride(kind) {
            return Err(malformed(format!("{kind:?} has the wrong record size")));
        }
        let rows = entry.count.get();
        if rows == 0 || rows > MAX_ROWS {
            return Err(malformed(format!("{kind:?} has {rows} rows")));
        }
        let table_length = (stride as u64)
            .checked_mul(rows)
            .filter(|table_length| *table_length == entry.length.get())
            .ok_or_else(|| malformed(format!("{kind:?}'s length disagrees with its rows")))?;
        let offset = entry.offset.get();
        let end = offset
            .checked_add(table_length)
            .filter(|end| *end <= trailer_start as u64)
            .ok_or(ImageError::Truncated)?;
        let (Ok(offset), Ok(end)) = (usize::try_from(offset), usize::try_from(end)) else {
            return Err(ImageError::TooLarge);
        };
        if offset % format::TABLE_ALIGNMENT != 0 || offset < previous_end {
            return Err(malformed(format!("{kind:?} is misplaced")));
        }
        if bytes[previous_end..offset].iter().any(|byte| *byte != 0) {
            return Err(malformed("padding between tables is not zero"));
        }
        previous_end = end;
        tables[kind.index()] = Some(Placed {
            offset,
            length: end - offset,
        });
    }
    if bytes[previous_end..trailer_start]
        .iter()
        .any(|byte| *byte != 0)
    {
        return Err(malformed("padding before the trailer is not zero"));
    }
    Ok(tables)
}

/// Checks every table's contents.
pub(super) fn contents(image: &Image) -> Result<(), ImageError> {
    let strings = image.bytes(TableKind::Strings);
    if !strings::valid_pool(strings, true) {
        return Err(malformed("the string pool is not NUL-terminated UTF-8"));
    }
    let paths = image.bytes(TableKind::Paths);
    if !strings::valid_pool(paths, false) {
        return Err(malformed("the path pool is not NUL-terminated"));
    }
    let files = image.table::<FileRecord>();
    if files
        .iter()
        .any(|file| !strings::valid_reference(paths, file.path.get(), false))
    {
        return Err(malformed("a file names no path"));
    }
    let phase = crate::span!("validate.lines");
    let tables = Lines {
        addresses: image.table(),
        rows: image.table(),
        extras: image.table(),
        sequences: image.table(),
        ranges: image.table(),
    };
    tables.rows(files.len())?;
    tables.extras()?;
    tables.sequences()?;
    tables.ranges()?;
    tables.statements(image.table())?;
    tables.boundaries(image.table())?;
    drop(phase);
    let families: [(&str, Validation); 6] = [
        ("validate.symbols", super::symbols::validate),
        ("validate.functions", super::functions::validate),
        ("validate.unwind", super::unwind::validate),
        ("validate.facts", super::facts::validate),
        ("validate.packages", super::packages::validate),
        ("validate.types", super::types::validate),
    ];
    for (name, validate) in families {
        let _phase = crate::profile::span(name, None, None);
        validate(image).map_err(malformed)?;
    }
    Ok(())
}

/// The line tables, checked against each other and their indexes. Each
/// index is strictly ordered, refers to records it agrees with, and has as
/// many entries as there are records it indexes: so it indexes each
/// exactly once.
struct Lines<'a> {
    addresses: &'a [RowAddress],
    rows: &'a [LineRow],
    extras: &'a [LineExtra],
    sequences: &'a [LineSequence],
    ranges: &'a [LineRange],
}

/// Whether `items`' keys strictly increase.
fn strictly_ordered<T, K: Ord>(items: &[T], key: impl Fn(&T) -> K) -> bool {
    items.is_sorted_by(|earlier, later| key(earlier) < key(later))
}

impl Lines<'_> {
    fn rows(&self, files: usize) -> Result<(), ImageError> {
        if self.addresses.len() != self.rows.len() {
            return Err(malformed("rows and their addresses differ in number"));
        }
        let mut extras = 0;
        for row in self.rows {
            let file = row.file.get();
            if file != NONE && file as usize >= files {
                return Err(malformed("a row names a file that does not exist"));
            }
            if row.flags & !row_flags::ALL != 0 || row.reserved != 0 {
                return Err(malformed("a row has flags it does not define"));
            }
            let wide = row.line.get() == lines::WIDE_LINE || row.column.get() == lines::WIDE_COLUMN;
            if row.flags & row_flags::EXTRA != 0 {
                extras += 1;
            } else if wide {
                return Err(malformed("a row's wide line or column has no extra"));
            }
        }
        if extras != self.extras.len() {
            return Err(malformed("rows and their extras differ in number"));
        }
        Ok(())
    }

    /// Each extra belongs to a row that says it has one, and holds what the
    /// row cannot, so that each row has one encoding.
    fn extras(&self) -> Result<(), ImageError> {
        if !strictly_ordered(self.extras, |extra| extra.row.get()) {
            return Err(malformed("row extras are out of order"));
        }
        // The row holds its line and column whole unless they are too
        // wide, and its extra repeats them.
        let agrees = |narrow: u64, wide: u64, whole: u64| {
            if narrow == wide {
                whole >= wide
            } else {
                whole == narrow
            }
        };
        for extra in self.extras {
            let row = self
                .rows
                .get(extra.row.get() as usize)
                .filter(|row| row.flags & row_flags::EXTRA != 0)
                .ok_or_else(|| malformed("an extra belongs to no row that has one"))?;
            let line = u64::from(row.line.get());
            let column = u64::from(row.column.get());
            if !agrees(line, u64::from(lines::WIDE_LINE), extra.line.get())
                || !agrees(column, u64::from(lines::WIDE_COLUMN), extra.column.get())
            {
                return Err(malformed("an extra disagrees with its row"));
            }
            let needed = row.line.get() == lines::WIDE_LINE
                || row.column.get() == lines::WIDE_COLUMN
                || extra.operation_index.get() != 0
                || extra.discriminator.get() != 0
                || extra.isa.get() != 0;
            if !needed {
                return Err(malformed("a row has an extra it does not need"));
            }
        }
        Ok(())
    }

    /// The sequences cover the rows in order, each ending at its last row.
    fn sequences(&self) -> Result<(), ImageError> {
        let mut next = 0_usize;
        for sequence in self.sequences {
            let (first, count) = (sequence.first.get() as usize, sequence.rows.get() as usize);
            if first != next || count == 0 {
                return Err(malformed("sequences do not cover the rows in order"));
            }
            next = first
                .checked_add(count)
                .filter(|end| *end <= self.rows.len())
                .ok_or_else(|| malformed("a sequence runs past the rows"))?;
            if self.rows[first..next - 1]
                .iter()
                .any(|row| row.flags & row_flags::END_SEQUENCE != 0)
            {
                return Err(malformed("a sequence ends before its last row"));
            }
        }
        if next != self.rows.len() {
            return Err(malformed("rows lie outside every sequence"));
        }
        Ok(())
    }

    fn ranges(&self) -> Result<(), ImageError> {
        let located = |row: u32| self.rows.get(row as usize).is_some_and(lines::is_located);
        for range in self.ranges {
            if range.start.get() >= range.end.get()
                || range.statement > 1
                || !located(range.row.get())
            {
                return Err(malformed("a line range is empty or names no located row"));
            }
        }
        if !strictly_ordered(self.ranges, |range| {
            (range.start.get(), range.end.get(), range.row.get())
        }) {
            return Err(malformed("the line ranges are out of order"));
        }
        let mut prefix_max_end = 0;
        for range in self.ranges {
            prefix_max_end = prefix_max_end.max(range.end.get());
            if range.prefix_max_end.get() != prefix_max_end {
                return Err(malformed("the line ranges' running ends are wrong"));
            }
        }
        Ok(())
    }

    fn statements(&self, statements: &[StatementKey]) -> Result<(), ImageError> {
        let indexed = |row: &LineRow| {
            lines::is_public(row) && row.flags & row_flags::STATEMENT != 0 && lines::is_located(row)
        };
        if statements.len() != self.rows.iter().filter(|row| indexed(row)).count() {
            return Err(malformed("the statement index misses statements"));
        }
        if !strictly_ordered(statements, |key| {
            (key.file.get(), key.line.get(), key.row.get())
        }) {
            return Err(malformed("the statement index is out of order"));
        }
        for key in statements {
            let at = key.row.get() as usize;
            let agrees = self.rows.get(at).is_some_and(|row| {
                indexed(row)
                    && row.file == key.file
                    && lines::decode_row(self.addresses, self.rows, self.extras, at).line
                        == key.line.get()
            });
            if !agrees {
                return Err(malformed("the statement index disagrees with the rows"));
            }
        }
        Ok(())
    }

    fn boundaries(&self, boundaries: &[ControlBoundary]) -> Result<(), ImageError> {
        let marks = |row: &LineRow| {
            lines::is_public(row)
                && row.flags & (row_flags::PROLOGUE_END | row_flags::EPILOGUE_BEGIN) != 0
        };
        if boundaries.len() != self.rows.iter().filter(|row| marks(row)).count() {
            return Err(malformed("the boundary index misses boundaries"));
        }
        if !boundaries.iter().all(|boundary| {
            self.rows
                .get(boundary.row.get() as usize)
                .is_some_and(marks)
        }) {
            return Err(malformed("the boundary index names a row that marks none"));
        }
        if !strictly_ordered(boundaries, |boundary| {
            let at = boundary.row.get();
            (self.addresses[at as usize].address.get(), at)
        }) {
            return Err(malformed("the boundary index is out of order"));
        }
        Ok(())
    }
}
