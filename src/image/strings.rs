//! Pools of NUL-terminated strings, named by offset.
//!
//! [`StrId`]s name display strings, which are UTF-8; [`PathId`]s name
//! filesystem paths, whose bytes need not be. Each pool is validated once
//! as a whole, so a reference is valid when it starts a string or lies on
//! a character boundary within one: a suffix of a valid string is valid.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

/// A display string, by its offset in the string pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StrId(pub u32);

/// Display strings being pooled, each where it is first pushed.
#[derive(Debug, Default)]
pub struct StringsBuilder {
    bytes: Vec<u8>,
}

impl StringsBuilder {
    /// The pool's name for `text`, or `None` when it holds a NUL or the
    /// pool is full.
    pub fn push(&mut self, text: &str) -> Option<StrId> {
        if text.as_bytes().contains(&0) {
            return None;
        }
        let id = StrId(u32::try_from(self.bytes.len()).ok()?);
        self.bytes.extend_from_slice(text.as_bytes());
        self.bytes.push(0);
        u32::try_from(self.bytes.len()).ok()?;
        Some(id)
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// The string pool of a validated image, or of a builder.
#[derive(Debug, Clone, Copy)]
pub struct Strings<'a>(pub &'a [u8]);

impl<'a> Strings<'a> {
    /// The string `id` names. Validation checked that it is terminated
    /// UTF-8.
    pub fn get(self, id: StrId) -> &'a str {
        std::str::from_utf8(self.bytes(id)).expect("validation checked every string is UTF-8")
    }

    /// The bytes of the string `id` names, without its NUL.
    pub fn bytes(self, id: StrId) -> &'a [u8] {
        let rest = &self.0[id.0 as usize..];
        let end = memchr::memchr(0, rest).expect("validation checked every string is terminated");
        &rest[..end]
    }

    /// Whether `id` names a string of the pool.
    pub fn contains(self, id: StrId) -> bool {
        valid_reference(self.0, id.0, true)
    }
}

/// A filesystem path, by its offset in the path pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PathId(pub u32);

/// Paths being interned, each once.
#[derive(Debug, Default)]
pub struct PathsBuilder {
    bytes: Vec<u8>,
    ids: foldhash::HashMap<Box<[u8]>, PathId>,
}

impl PathsBuilder {
    /// The pool's name for `path`, which must not contain a NUL, as no
    /// Unix path does.
    pub fn intern(&mut self, path: &Path) -> Option<PathId> {
        let bytes = path.as_os_str().as_bytes();
        if bytes.contains(&0) {
            return None;
        }
        if let Some(&id) = self.ids.get(bytes) {
            return Some(id);
        }
        let id = PathId(u32::try_from(self.bytes.len()).ok()?);
        self.bytes.extend_from_slice(bytes);
        self.bytes.push(0);
        u32::try_from(self.bytes.len()).ok()?;
        self.ids.insert(bytes.into(), id);
        Some(id)
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// The path pool of a validated image.
#[derive(Debug, Clone, Copy)]
pub struct Paths<'a>(pub &'a [u8]);

impl<'a> Paths<'a> {
    /// The path `id` names. Validation checked that it is terminated.
    pub fn get(self, id: PathId) -> &'a Path {
        let start = id.0 as usize;
        let rest = &self.0[start..];
        let end = memchr::memchr(0, rest).expect("validation checked every path is terminated");
        Path::new(OsStr::from_bytes(&rest[..end]))
    }
}

/// Whether `pool` is NUL-terminated strings, each valid UTF-8 when `utf8`.
pub(super) fn valid_pool(pool: &[u8], utf8: bool) -> bool {
    if pool.last().is_some_and(|last| *last != 0) {
        return false;
    }
    !utf8
        || pool
            .split(|byte| *byte == 0)
            .all(|string| std::str::from_utf8(string).is_ok())
}

/// Whether `offset` names a string in a valid pool: inside it, and, for a
/// UTF-8 pool, at a character boundary.
pub(super) fn valid_reference(pool: &[u8], offset: u32, utf8: bool) -> bool {
    let Some(&byte) = pool.get(offset as usize) else {
        return false;
    };
    // Continuation bytes are 0b10xx_xxxx.
    !utf8 || byte & 0xc0 != 0x80
}
