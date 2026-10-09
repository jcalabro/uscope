//! A module's static metadata as one buffer of typed tables.
//!
//! An image is a header, a directory, and a fixed set of tables, each a
//! slice of one little-endian record type that nothing in the buffer
//! points into: records refer to each other by index. The bytes in memory
//! are the bytes a cache writes to disk, and an image read back is
//! validated completely before any view reads it, so that after
//! validation every lookup is an in-bounds index.
//!
//! The container and its tables are neutral: lines, files, and the other
//! debugger concepts. What a provider needs that no other part of the
//! debugger reads stays in the provider.

pub mod backing;
pub mod calls;
pub mod declarations;
pub mod facts;
mod format;
pub mod functions;
pub mod index;
pub mod lines;
pub mod locations;
pub mod packages;
pub mod resumes;
mod schema;
mod strings;
pub mod symbols;
pub mod type_facts;
pub mod types;
pub mod unwind;
mod validate;
pub mod variables;

#[cfg(any(test, feature = "fuzzing"))]
pub mod sample;
#[cfg(test)]
mod tests;

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

pub use backing::AlignedBytes;
pub use format::TableKind;
pub use strings::{PathId, Paths, PathsBuilder, Strings, StringsBuilder};
pub use validate::{ImageError, Limits};

use format::{DirectoryEntry, Header, Trailer};

/// The index that stands for no row in any table's reference column.
pub const NONE: u32 = u32::MAX;

/// The most rows one table may hold: every index but [`NONE`].
pub const MAX_ROWS: u64 = NONE as u64;

/// One record type: the table it fills.
pub trait Record: FromBytes + IntoBytes + KnownLayout + Immutable + Unaligned {
    const KIND: TableKind;
}

/// A record type several tables hold, such as an index's entries.
pub trait SharedRecord: FromBytes + IntoBytes + KnownLayout + Immutable + Unaligned {
    /// The record's name in the schema.
    const NAME: &'static str;
}

/// Where one table is in an image.
#[derive(Debug, Clone, Copy)]
struct Placed {
    offset: usize,
    length: usize,
}

/// What an image's bytes mean beyond the inputs they were built from: the
/// format, the normalization revision, and every record's layout.
pub fn revisions() -> [u64; 3] {
    [
        format::FORMAT_VERSION.into(),
        format::NORMALIZATION_REVISION.into(),
        schema::layout_fingerprint(),
    ]
}

/// Validated, immutable tables over owned bytes.
#[derive(Debug)]
pub struct Image {
    bytes: AlignedBytes,
    tables: [Option<Placed>; TableKind::COUNT],
}

impl Image {
    /// Validates `bytes` as an image and takes them, or reports the first
    /// problem found.
    pub fn from_bytes(bytes: AlignedBytes, limits: Limits) -> Result<Self, ImageError> {
        let tables = validate::structure(&bytes, limits)?;
        let image = Self { bytes, tables };
        validate::contents(&image)?;
        Ok(image)
    }

    /// The target the image describes.
    pub fn target(&self) -> crate::TargetDescription {
        let (header, _) = format::Header::ref_from_prefix(self.as_bytes())
            .expect("validation checked the header");
        schema::target_of(header).expect("validation checked the target")
    }

    /// The whole image, as a cache writes it.
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_bytes()
    }

    /// The rows of `T`'s table: empty when the image has none.
    pub fn table<T: Record>(&self) -> &[T] {
        let Some(placed) = self.tables[T::KIND.index()] else {
            return &[];
        };
        <[T]>::ref_from_bytes(&self.bytes[placed.offset..placed.offset + placed.length])
            .expect("validation checked every table's length and stride")
    }

    /// The rows of the table of `kind`, which holds `T`s: empty when the
    /// image has none.
    pub fn shared<T: SharedRecord>(&self, kind: TableKind) -> &[T] {
        assert_eq!(
            schema::record(kind),
            T::NAME,
            "{kind:?} holds another record"
        );
        let Some(placed) = self.tables[kind.index()] else {
            return &[];
        };
        <[T]>::ref_from_bytes(&self.bytes[placed.offset..placed.offset + placed.length])
            .expect("validation checked every table's length and stride")
    }

    /// The string pool.
    pub fn strings(&self) -> Strings<'_> {
        Strings(self.bytes(TableKind::Strings))
    }

    /// The bytes of a byte table, such as the string pool.
    pub fn bytes(&self, kind: TableKind) -> &[u8] {
        self.tables[kind.index()].map_or(&[], |placed| {
            &self.bytes[placed.offset..placed.offset + placed.length]
        })
    }
}

/// Assembles tables into an image.
pub struct Builder {
    target: crate::TargetDescription,
    tables: Vec<(TableKind, usize, usize, Vec<u8>)>,
}

impl Builder {
    pub const fn new(target: crate::TargetDescription) -> Self {
        Self {
            target,
            tables: Vec::new(),
        }
    }

    /// Adds `T`'s table. Empty tables are left out.
    pub fn table<T: Record>(&mut self, rows: &[T]) -> &mut Self {
        if !rows.is_empty() {
            self.add(
                T::KIND,
                size_of::<T>(),
                rows.len(),
                rows.as_bytes().to_vec(),
            );
        }
        self
    }

    /// Adds the table of `kind`, which holds `T`s. Empty tables are left
    /// out.
    pub fn shared<T: SharedRecord>(&mut self, kind: TableKind, rows: &[T]) -> &mut Self {
        assert_eq!(
            schema::record(kind),
            T::NAME,
            "{kind:?} holds another record"
        );
        if !rows.is_empty() {
            self.add(kind, size_of::<T>(), rows.len(), rows.as_bytes().to_vec());
        }
        self
    }

    /// Adds a byte table, such as the string pool.
    pub fn bytes(&mut self, kind: TableKind, bytes: Vec<u8>) -> &mut Self {
        if !bytes.is_empty() {
            let length = bytes.len();
            self.add(kind, 1, length, bytes);
        }
        self
    }

    fn add(&mut self, kind: TableKind, stride: usize, count: usize, bytes: Vec<u8>) {
        assert!(
            self.tables.iter().all(|(added, ..)| *added != kind),
            "{kind:?} is added once"
        );
        self.tables.push((kind, stride, count, bytes));
    }

    /// Lays out the tables, seals them with the trailer's checksum, and
    /// validates the result as a reader would.
    pub fn seal(mut self, limits: Limits) -> Result<Image, ImageError> {
        self.tables.sort_by_key(|(kind, ..)| *kind);
        let directory_start = size_of::<Header>();
        let directory_length = self.tables.len() * size_of::<DirectoryEntry>();
        let mut offset =
            format::aligned(directory_start + directory_length).ok_or(ImageError::TooLarge)?;
        let mut entries = Vec::with_capacity(self.tables.len());
        let mut offsets = Vec::with_capacity(self.tables.len());
        for (kind, stride, count, bytes) in &self.tables {
            offsets.push(offset);
            entries.push(DirectoryEntry {
                kind: (*kind as u32).into(),
                stride: u32::try_from(*stride)
                    .map_err(|_| ImageError::TooLarge)?
                    .into(),
                count: (*count as u64).into(),
                offset: (offset as u64).into(),
                length: (bytes.len() as u64).into(),
            });
            offset = format::aligned(
                offset
                    .checked_add(bytes.len())
                    .ok_or(ImageError::TooLarge)?,
            )
            .ok_or(ImageError::TooLarge)?;
        }
        let length = offset
            .checked_add(size_of::<Trailer>())
            .ok_or(ImageError::TooLarge)?;
        let mut header = schema::header(self.target, length, entries.len())?;
        header.magic = format::MAGIC;
        let mut bytes = AlignedBytes::zeroed(length).ok_or(ImageError::TooLarge)?;
        let buffer = bytes.as_mut_bytes();
        buffer[..directory_start].copy_from_slice(header.as_bytes());
        buffer[directory_start..directory_start + directory_length]
            .copy_from_slice(entries.as_bytes());
        for ((_, _, _, table), start) in self.tables.iter().zip(offsets) {
            buffer[start..start + table.len()].copy_from_slice(table);
        }
        let checksum = twox_hash::XxHash3_64::oneshot(&buffer[..offset]);
        buffer[offset..].copy_from_slice(
            Trailer {
                checksum: checksum.into(),
            }
            .as_bytes(),
        );
        Image::from_bytes(bytes, limits)
    }
}
