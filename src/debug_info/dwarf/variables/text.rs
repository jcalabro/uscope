//! The text string values hold: C strings, character arrays, and the
//! string types of Rust, Go, and C++'s libstdc++.
//!
//! Text is read for display, bounded per value and never across a page that
//! cannot be read, so a corrupt pointer or length costs at most a few reads.
//! It is not charged to an inspection's budget, which bounds the values
//! themselves.

use std::sync::Arc;

use crate::model::{TextCompletion, TextSummary};
use crate::{
    BaseTypeEncoding, RecordMember, RecordMemberLayout, TypeId, VariableValue, VirtualAddress,
};

use super::codec::{decode_address, unsigned_value};
use super::inspect::LocatedStorage;
use super::shape::{ValueShape, ValueShapeKind};
use super::{DwarfVariableInfo, VariableRuntime};

const PAGE_SIZE: u64 = 4096;
/// How deep a string type's pointer to its bytes may be nested.
const MAX_STRING_DEPTH: usize = 6;

impl DwarfVariableInfo {
    /// The text a value of this shape holds, when it is a string.
    pub(super) fn text_summary(
        &self,
        shape: &ValueShape,
        value: &VariableValue,
        storage: &LocatedStorage,
        runtime: &mut dyn VariableRuntime,
    ) -> Option<TextSummary> {
        match (&shape.kind, value) {
            (
                ValueShapeKind::Indirection {
                    target: Some(target),
                    ..
                },
                VariableValue::Address(address),
            ) if address.address.get() != 0 && self.is_character(*target) => {
                Some(c_string(runtime, address.address))
            }
            (
                ValueShapeKind::Array {
                    element,
                    dimensions,
                    ..
                },
                _,
            ) if dimensions.len() == 1 && self.is_character(*element) => {
                let count = usize::try_from(dimensions[0].count).ok()?;
                let bytes = storage_bytes(storage, 0, count.min(TextSummary::MAX_BYTES), runtime)?;
                let terminated = bytes.iter().position(|byte| *byte == 0);
                Some(TextSummary {
                    bytes: Arc::from(&bytes[..terminated.unwrap_or(bytes.len())]),
                    completion: if terminated.is_none() && count > TextSummary::MAX_BYTES {
                        TextCompletion::Truncated { length: None }
                    } else {
                        TextCompletion::Complete
                    },
                })
            }
            (
                ValueShapeKind::Record {
                    record, members, ..
                },
                _,
            ) => self.record_text(*record, members, storage, runtime),
            // A pointer or reference to a string shows the string's text.
            (
                ValueShapeKind::Indirection {
                    target: Some(target),
                    ..
                },
                VariableValue::Address(address),
            ) if address.address.get() != 0 => match self.value_shape(*target).ok()?.kind {
                ValueShapeKind::Record {
                    record, members, ..
                } => self.record_text(
                    record,
                    &members,
                    &LocatedStorage::Memory(address.address),
                    runtime,
                ),
                _ => None,
            },
            _ => None,
        }
    }

    /// The text of a record that is a string type.
    fn record_text(
        &self,
        record: TypeId,
        members: &[RecordMember],
        storage: &LocatedStorage,
        runtime: &mut dyn VariableRuntime,
    ) -> Option<TextSummary> {
        let name = self.type_info(record).ok()?.name.clone();
        let (pointer, length) = self.string_parts(&name, members)?;
        let address = self.read_pointer(storage, pointer, runtime)?;
        let length = self.read_word(storage, length, runtime)?;
        Some(counted_text(runtime, address, length))
    }

    /// Whether a type is a one-byte character, through typedefs and
    /// qualifiers.
    fn is_character(&self, id: TypeId) -> bool {
        self.value_shape(id).is_ok_and(|shape| {
            shape.scalar().is_some_and(|base| {
                base.byte_size == 1
                    && matches!(
                        base.encoding,
                        BaseTypeEncoding::SignedCharacter | BaseTypeEncoding::UnsignedCharacter
                    )
            })
        })
    }

    /// The byte offsets of a string type's pointer to its bytes and of its
    /// length, for the string types whose layout is known.
    fn string_parts(&self, name: &str, members: &[RecordMember]) -> Option<(u64, u64)> {
        let at = |path: &[&str]| self.member_path(members, path);
        if name == "&str" || name == "&mut str" {
            return Some((at(&["data_ptr"])?, at(&["length"])?));
        }
        if name == "string" {
            // Go's string header.
            return Some((at(&["str"])?, at(&["len"])?));
        }
        // Records are named without their namespaces: Rust's
        // `alloc::string::String` is `String`.
        if name == "String" {
            // Rust's String wraps a Vec<u8>, whose buffer type changes
            // between releases; its one pointer is the bytes.
            let (buffer, buffer_type) = member(members, "vec").and_then(|(vec, vec_type)| {
                let (offset, id) = member(&self.record_members(vec_type)?, "buf")?;
                Some((vec + offset, id))
            })?;
            let length = at(&["vec", "len"])?;
            return Some((buffer + self.first_pointer(buffer_type, 0)?, length));
        }
        // libstdc++'s `std::string`, when the program's debug information
        // defines it rather than only declaring it.
        if name.starts_with("basic_string<char,") {
            return Some((at(&["_M_dataplus", "_M_p"])?, at(&["_M_string_length"])?));
        }
        None
    }

    fn record_members(&self, id: TypeId) -> Option<Arc<[RecordMember]>> {
        match self.value_shape(id).ok()?.kind {
            ValueShapeKind::Record { members, .. } => Some(members),
            _ => None,
        }
    }

    /// The byte offset of a nested member.
    fn member_path(&self, members: &[RecordMember], path: &[&str]) -> Option<u64> {
        let (first, rest) = path.split_first()?;
        let (offset, id) = member(members, first)?;
        if rest.is_empty() {
            return Some(offset);
        }
        Some(offset + self.member_path(&self.record_members(id)?, rest)?)
    }

    /// The byte offset of the first pointer within a type.
    fn first_pointer(&self, id: TypeId, depth: usize) -> Option<u64> {
        if depth > MAX_STRING_DEPTH {
            return None;
        }
        match self.value_shape(id).ok()?.kind {
            ValueShapeKind::Indirection { .. } => Some(0),
            ValueShapeKind::Record { members, .. } => members.iter().find_map(|member| {
                let RecordMemberLayout::ByteOffset(offset) = member.layout else {
                    return None;
                };
                Some(offset + self.first_pointer(member.type_ref.id, depth + 1)?)
            }),
            _ => None,
        }
    }

    const fn pointer_bytes(&self) -> usize {
        match self.target.pointer_width {
            crate::PointerWidth::Bits32 => 4,
            crate::PointerWidth::Bits64 => 8,
        }
    }

    fn read_pointer(
        &self,
        storage: &LocatedStorage,
        offset: u64,
        runtime: &mut dyn VariableRuntime,
    ) -> Option<VirtualAddress> {
        let size = self.pointer_bytes();
        let raw = storage_bytes(storage, offset, size, runtime)?;
        decode_address(&raw, size as u64, self.target).ok()
    }

    fn read_word(
        &self,
        storage: &LocatedStorage,
        offset: u64,
        runtime: &mut dyn VariableRuntime,
    ) -> Option<u64> {
        let raw = storage_bytes(storage, offset, self.pointer_bytes(), runtime)?;
        u64::try_from(unsigned_value(&raw, self.target.byte_order).ok()?).ok()
    }
}

/// A member's byte offset and type.
fn member(members: &[RecordMember], name: &str) -> Option<(u64, TypeId)> {
    let member = members
        .iter()
        .find(|member| member.name.as_deref() == Some(name))?;
    match member.layout {
        RecordMemberLayout::ByteOffset(offset) => Some((offset, member.type_ref.id)),
        _ => None,
    }
}

/// Reads `size` bytes at `offset` within a value's storage.
fn storage_bytes(
    storage: &LocatedStorage,
    offset: u64,
    size: usize,
    runtime: &mut dyn VariableRuntime,
) -> Option<Vec<u8>> {
    match storage {
        LocatedStorage::Memory(address) => {
            let address = VirtualAddress::new(address.get().checked_add(offset)?);
            runtime
                .read_memory(address, size)
                .ok()
                .map(|bytes| bytes.to_vec())
        }
        LocatedStorage::Bytes {
            raw, start, end, ..
        } => {
            let first = start.checked_add(usize::try_from(offset).ok()?)?;
            let last = first.checked_add(size)?;
            (last <= *end).then(|| raw[first..last].to_vec())
        }
        LocatedStorage::ImplicitPointer { .. } => None,
    }
}

/// Reads up to `limit` bytes at `address`, a page at a time, stopping at
/// the first byte `stop` accepts. Returns the bytes before it, whether it
/// was found, and the first unreadable address when reading failed.
fn read_text(
    runtime: &mut dyn VariableRuntime,
    address: VirtualAddress,
    limit: usize,
    stop: impl Fn(u8) -> bool,
) -> (Vec<u8>, bool, Option<VirtualAddress>) {
    let mut bytes = Vec::new();
    while bytes.len() < limit {
        let Some(next) = address.get().checked_add(bytes.len() as u64) else {
            return (bytes, false, None);
        };
        let page_end = (next / PAGE_SIZE + 1) * PAGE_SIZE;
        let size = usize::try_from(page_end - next)
            .unwrap_or(usize::MAX)
            .min(limit - bytes.len());
        let Ok(chunk) = runtime.read_memory(VirtualAddress::new(next), size) else {
            return (bytes, false, Some(VirtualAddress::new(next)));
        };
        if let Some(position) = chunk.iter().position(|byte| stop(*byte)) {
            bytes.extend_from_slice(&chunk[..position]);
            return (bytes, true, None);
        }
        bytes.extend_from_slice(&chunk);
    }
    (bytes, false, None)
}

/// The NUL-terminated text at an address.
fn c_string(runtime: &mut dyn VariableRuntime, address: VirtualAddress) -> TextSummary {
    let (bytes, terminated, unreadable) =
        read_text(runtime, address, TextSummary::MAX_BYTES, |byte| byte == 0);
    TextSummary {
        bytes: bytes.into(),
        completion: match unreadable {
            Some(address) => TextCompletion::Unreadable { address },
            None if terminated => TextCompletion::Complete,
            None => TextCompletion::Truncated { length: None },
        },
    }
}

/// The `length` bytes of text at an address.
fn counted_text(
    runtime: &mut dyn VariableRuntime,
    address: VirtualAddress,
    length: u64,
) -> TextSummary {
    let limit = usize::try_from(length)
        .unwrap_or(usize::MAX)
        .min(TextSummary::MAX_BYTES);
    let (bytes, _, unreadable) = read_text(runtime, address, limit, |_| false);
    TextSummary {
        bytes: bytes.into(),
        completion: match unreadable {
            Some(address) => TextCompletion::Unreadable { address },
            None if length > limit as u64 => TextCompletion::Truncated {
                length: Some(length),
            },
            None => TextCompletion::Complete,
        },
    }
}
