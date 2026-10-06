//! The text string values hold: C strings, character arrays, the slices a
//! language makes its text of, and Go's strings. The string classes of
//! libraries are views (`views/`).
//!
//! Text is read for display, bounded per value and never across a page that
//! cannot be read, so a corrupt pointer or length costs at most a few reads.
//! Every read is charged to the inspection's budget. Text the budget cannot
//! afford is cut short and says so, without failing the rest of the
//! inspection.

use std::sync::Arc;

use crate::inspection::InspectionBudget;
use crate::model::{TextCompletion, TextSummary, ValueStorage};
use crate::{
    BaseTypeEncoding, GoKind, InspectionExhaustion, RecordMember, RecordMemberLayout,
    SourceLanguage, TypeId, TypeKind, VariableValue, VirtualAddress,
};

use super::codec::{decode_address, unsigned_value};
use super::pieces::unavailable_within;
use super::shape::ValueShape;
use super::{DwarfVariableInfo, VariableRuntime};

const PAGE_SIZE: u64 = 4096;
/// How deep a string type's pointer to its bytes may be nested.
const MAX_STRING_DEPTH: usize = 6;

/// Why reading text stopped before its end.
enum Stopped {
    /// Memory at this address could not be read.
    Unreadable(VirtualAddress),
    /// The budget could not afford the next read.
    Limited(InspectionExhaustion),
}

/// Reads target memory for text, charging the inspection's budget.
struct TextReader<'a> {
    runtime: &'a mut dyn VariableRuntime,
    budget: &'a mut InspectionBudget,
}

impl TextReader<'_> {
    /// Reads exactly `size` bytes, or nothing when the budget cannot afford
    /// them all.
    fn read_exact(&mut self, address: VirtualAddress, size: usize) -> Result<Vec<u8>, Stopped> {
        if let Some(exhaustion) = self.budget.memory_shortfall(size as u64) {
            return Err(Stopped::Limited(exhaustion));
        }
        self.budget.consume_memory(size).map_err(Stopped::Limited)?;
        self.runtime
            .read_memory(address, size)
            .map(|bytes| bytes.to_vec())
            .map_err(|_| Stopped::Unreadable(address))
    }

    /// Reads up to `limit` bytes at `address`, a page at a time, stopping
    /// at the first byte `stop` accepts. Returns the bytes before it,
    /// whether it was found, and why reading stopped short, if it did.
    fn read_text(
        &mut self,
        address: VirtualAddress,
        limit: usize,
        stop: impl Fn(u8) -> bool,
    ) -> (Vec<u8>, bool, Option<Stopped>) {
        let mut bytes = Vec::new();
        while bytes.len() < limit {
            let Some(next) = address.get().checked_add(bytes.len() as u64) else {
                return (bytes, false, None);
            };
            // The bytes left in this page, which the last page of the
            // address space ends without overflowing.
            let size = usize::try_from(PAGE_SIZE - next % PAGE_SIZE)
                .unwrap_or(usize::MAX)
                .min(limit - bytes.len());
            let granted = match self.budget.consume_memory_up_to(size) {
                Ok(granted) => granted,
                Err(exhaustion) => return (bytes, false, Some(Stopped::Limited(exhaustion))),
            };
            let next = VirtualAddress::new(next);
            let Ok(chunk) = self.runtime.read_memory(next, granted) else {
                return (bytes, false, Some(Stopped::Unreadable(next)));
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
    fn c_string(&mut self, address: VirtualAddress) -> TextSummary {
        let (bytes, terminated, stopped) =
            self.read_text(address, TextSummary::MAX_BYTES, |byte| byte == 0);
        TextSummary {
            bytes: bytes.into(),
            completion: match stopped {
                Some(stopped) => stopped.completion(None),
                None if terminated => TextCompletion::Complete,
                None => TextCompletion::Truncated { length: None },
            },
        }
    }

    /// The `length` bytes of text at an address.
    fn counted_text(&mut self, address: VirtualAddress, length: u64) -> TextSummary {
        let limit = usize::try_from(length)
            .unwrap_or(usize::MAX)
            .min(TextSummary::MAX_BYTES);
        let (bytes, _, stopped) = self.read_text(address, limit, |_| false);
        TextSummary {
            bytes: bytes.into(),
            completion: match stopped {
                Some(stopped) => stopped.completion(Some(length)),
                None if length > limit as u64 => TextCompletion::Truncated {
                    length: Some(length),
                },
                None => TextCompletion::Complete,
            },
        }
    }

    /// Reads `size` bytes at `offset` within a value's storage.
    fn storage_bytes(
        &mut self,
        storage: &ValueStorage,
        offset: u64,
        size: usize,
    ) -> Option<Result<Vec<u8>, Stopped>> {
        match storage {
            ValueStorage::Memory(address) => {
                let address = VirtualAddress::new(address.get().checked_add(offset)?);
                Some(self.read_exact(address, size))
            }
            ValueStorage::Bytes {
                raw,
                start,
                end,
                unavailable,
                ..
            } => {
                let first = start.checked_add(usize::try_from(offset).ok()?)?;
                let last = first.checked_add(size)?;
                (last <= *end && unavailable_within(unavailable, first, size).is_none())
                    .then(|| Ok(raw[first..last].to_vec()))
            }
            ValueStorage::ImplicitPointer { .. } => None,
        }
    }
}

impl Stopped {
    const fn completion(self, length: Option<u64>) -> TextCompletion {
        match self {
            Self::Unreadable(address) => TextCompletion::Unreadable { address },
            Self::Limited(exhaustion) => TextCompletion::Limited { length, exhaustion },
        }
    }

    /// Text that stopped before its first byte.
    fn summary(self, length: Option<u64>) -> TextSummary {
        TextSummary {
            bytes: Arc::from([]),
            completion: self.completion(length),
        }
    }
}

impl DwarfVariableInfo {
    /// The text a value of this type and shape holds, when it is a string.
    pub(super) fn text_summary(
        &self,
        type_id: TypeId,
        shape: &ValueShape,
        value: &VariableValue,
        storage: &ValueStorage,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Option<TextSummary> {
        let mut reader = TextReader { runtime, budget };
        match (shape, value) {
            (
                ValueShape::Indirection {
                    target: Some(target),
                    ..
                },
                VariableValue::Address(address),
            ) if address.address.get() != 0
                && (self.is_character(*target) || self.is_sentinel_text(type_id)) =>
            {
                Some(reader.c_string(address.address))
            }
            (
                ValueShape::Array {
                    element,
                    dimensions,
                    ..
                },
                _,
            ) if dimensions.len() == 1 && self.is_character(*element) => {
                let count = usize::try_from(dimensions[0].count).ok()?;
                let bytes =
                    match reader.storage_bytes(storage, 0, count.min(TextSummary::MAX_BYTES))? {
                        Ok(bytes) => bytes,
                        Err(stopped) => return Some(stopped.summary(None)),
                    };
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
            (ValueShape::Slice { text: true, .. }, VariableValue::Slice { length, .. }) => {
                let address = match self.read_pointer(storage, 0, &mut reader)? {
                    Ok(address) => address,
                    Err(stopped) => return Some(stopped.summary(Some(*length))),
                };
                Some(reader.counted_text(address, *length))
            }
            (
                ValueShape::Record {
                    record, members, ..
                },
                _,
            ) => self.record_text(*record, members, storage, &mut reader),
            // A pointer or reference to a string shows the string's text.
            (
                ValueShape::Indirection {
                    target: Some(target),
                    ..
                },
                VariableValue::Address(address),
            ) if address.address.get() != 0 => {
                let storage = ValueStorage::Memory(address.address);
                match self.value_shape(*target).ok()? {
                    ValueShape::Record {
                        record, members, ..
                    } => self.record_text(record, &members, &storage, &mut reader),
                    ValueShape::Slice { text: true, .. } => {
                        let length = match self.read_word(
                            &storage,
                            self.pointer_bytes() as u64,
                            &mut reader,
                        )? {
                            Ok(length) => length,
                            Err(stopped) => return Some(stopped.summary(None)),
                        };
                        let address = match self.read_pointer(&storage, 0, &mut reader)? {
                            Ok(address) => address,
                            Err(stopped) => return Some(stopped.summary(Some(length))),
                        };
                        Some(reader.counted_text(address, length))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// The text of a record that is a string type.
    fn record_text(
        &self,
        record: TypeId,
        members: &[RecordMember],
        storage: &ValueStorage,
        reader: &mut TextReader<'_>,
    ) -> Option<TextSummary> {
        let (pointer, length) = self.string_parts(record, members)?;
        let address = match self.read_pointer(storage, pointer, reader)? {
            Ok(address) => address,
            Err(stopped) => return Some(stopped.summary(None)),
        };
        let length = match self.read_word(storage, length, reader)? {
            Ok(length) => length,
            Err(stopped) => return Some(stopped.summary(None)),
        };
        Some(reader.counted_text(address, length))
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

    /// Whether a type is Zig's NUL-terminated pointer to bytes,
    /// `[*:0]const u8`, through typedefs and qualifiers.
    fn is_sentinel_text(&self, id: TypeId) -> bool {
        let mut current = id;
        for _ in 0..MAX_STRING_DEPTH {
            let Ok(info) = self.type_info(current) else {
                return false;
            };
            match &info.kind {
                TypeKind::Modified { target, .. }
                | TypeKind::Named {
                    target: Some(target),
                    ..
                } => current = target.id,
                TypeKind::Pointer {
                    target: Some(target),
                    ..
                } => {
                    return info
                        .identity
                        .as_ref()
                        .is_some_and(|identity| identity.language == SourceLanguage::Zig)
                        && info.name.starts_with("[*:0]")
                        && self.value_shape(target.id).is_ok_and(|shape| {
                            shape.scalar().is_some_and(|base| {
                                base.byte_size == 1 && base.encoding == BaseTypeEncoding::Unsigned
                            })
                        });
                }
                _ => return false,
            }
        }
        false
    }

    /// The byte offsets of a string type's pointer to its bytes and of its
    /// length: Go's strings, whose kind says what they are. The string
    /// classes of libraries, whose layouts are private, are views
    /// (`views/`).
    fn string_parts(&self, record: TypeId, members: &[RecordMember]) -> Option<(u64, u64)> {
        let go = self.type_info(record).ok()?.identity.as_ref()?.go?;
        if go.kind != GoKind::String {
            return None;
        }
        Some((
            member_offset(members, "str")?,
            member_offset(members, "len")?,
        ))
    }

    pub(super) const fn pointer_bytes(&self) -> usize {
        self.target.pointer_width.bytes() as usize
    }

    fn read_pointer(
        &self,
        storage: &ValueStorage,
        offset: u64,
        reader: &mut TextReader<'_>,
    ) -> Option<Result<VirtualAddress, Stopped>> {
        let size = self.pointer_bytes();
        Some(match reader.storage_bytes(storage, offset, size)? {
            Ok(raw) => Ok(decode_address(&raw, size as u64, self.target).ok()?),
            Err(stopped) => Err(stopped),
        })
    }

    fn read_word(
        &self,
        storage: &ValueStorage,
        offset: u64,
        reader: &mut TextReader<'_>,
    ) -> Option<Result<u64, Stopped>> {
        Some(
            match reader.storage_bytes(storage, offset, self.pointer_bytes())? {
                Ok(raw) => {
                    Ok(u64::try_from(unsigned_value(&raw, self.target.byte_order).ok()?).ok()?)
                }
                Err(stopped) => Err(stopped),
            },
        )
    }
}

fn member_offset(members: &[RecordMember], name: &str) -> Option<u64> {
    let member = members
        .iter()
        .find(|member| member.name.as_deref() == Some(name))?;
    match member.layout {
        RecordMemberLayout::ByteOffset(offset) => Some(offset),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug_info::{VariableRegister, VariableRuntimeError};
    use crate::{ImageAddress, VariableUnavailableReason};

    /// Memory that holds a nonzero byte everywhere.
    struct Filled;

    impl VariableRuntime for Filled {
        fn register(&mut self, _: u16) -> Result<VariableRegister, VariableRuntimeError> {
            Err(VariableRuntimeError::Fatal("no registers".into()))
        }

        fn call_frame_cfa(&self) -> Result<VirtualAddress, VariableRuntimeError> {
            Err(VariableRuntimeError::Fatal("no frame".into()))
        }

        fn tls_address(&mut self, _: u64) -> Result<VirtualAddress, VariableUnavailableReason> {
            Err(crate::UnsupportedVariableFeature::Tls.into())
        }

        fn relocate(&self, address: ImageAddress) -> Result<VirtualAddress, Arc<str>> {
            Ok(VirtualAddress::new(address.get()))
        }

        fn image_address(&self, _: VirtualAddress) -> Option<ImageAddress> {
            None
        }

        fn read_memory(
            &mut self,
            _: VirtualAddress,
            size: usize,
        ) -> Result<Arc<[u8]>, VariableRuntimeError> {
            Ok(vec![b'x'; size].into())
        }
    }

    /// A corrupt pointer may point anywhere, up to the last byte of the
    /// address space, where the text ends without a terminator.
    #[test]
    fn text_reaching_the_end_of_the_address_space_is_truncated() {
        let mut budget = InspectionBudget::default();
        let mut reader = TextReader {
            runtime: &mut Filled,
            budget: &mut budget,
        };
        let text = reader.c_string(VirtualAddress::new(u64::MAX - 2));
        assert_eq!(&*text.bytes, b"xxx");
        assert_eq!(text.completion, TextCompletion::Truncated { length: None });
    }
}
