//! Value shapes: how a type's bytes are decoded and which children it has.

use std::collections::HashSet;
use std::sync::Arc;

use crate::model::ArrayDimension;
use crate::{
    BaseClass, BaseType, Enumerator, RecordMember, TypeId, TypeInfo, TypeKind, TypeModifier,
    TypeReference, Variant, VariantDiscriminant,
};

use super::codec::integer_bit_width;
use super::types::{TypeMetadataEntry, type_info_from};
use super::variant::is_single_default_variant;
use super::{MAX_SCALAR_BYTES, MAX_TYPE_RESOLUTION_DEPTH};

#[derive(Clone, Debug)]
pub(super) struct ValueShape {
    pub(super) kind: ValueShapeKind,
}

#[derive(Clone, Debug)]
pub(super) enum ValueShapeKind {
    Scalar(BaseType),
    Enumeration {
        representation: BaseType,
        enumerators: Arc<[Enumerator]>,
        byte_size: u64,
    },
    Array {
        element: TypeId,
        dimensions: Arc<[ArrayDimension]>,
        byte_size: u64,
    },
    Slice {
        element: TypeId,
        byte_size: u64,
        has_capacity: bool,
        text: bool,
    },
    Record {
        /// The canonical record DIE after aliases and qualifiers are removed.
        record: TypeId,
        members: Arc<[RecordMember]>,
        bases: Arc<[BaseClass]>,
        byte_size: u64,
    },
    Union {
        /// The canonical union DIE after aliases and qualifiers are removed.
        union: TypeId,
        members: Arc<[RecordMember]>,
        byte_size: u64,
    },
    Variant {
        /// The canonical aggregate DIE after aliases and qualifiers are removed.
        aggregate: TypeId,
        common_members: Arc<[RecordMember]>,
        bases: Arc<[BaseClass]>,
        discriminant: VariantDiscriminant,
        variants: Arc<[Variant]>,
        byte_size: u64,
    },
    Indirection {
        target: Option<TypeId>,
        byte_size: u64,
        address_class: u64,
    },
}

pub(super) enum TransparentRepresentationError {
    Malformed(Arc<str>),
    Unsupported(Arc<str>),
}

pub(super) fn transparent_representation<T: TypeMetadataEntry>(
    types: &[T],
    wrapper: &TypeInfo,
    target: TypeReference,
) -> std::result::Result<(), TransparentRepresentationError> {
    let target =
        type_info_from(types, target.id).map_err(TransparentRepresentationError::Malformed)?;
    if matches!(
        wrapper.kind,
        TypeKind::Modified {
            modifier: TypeModifier::Shared,
            ..
        }
    ) {
        return Err(TransparentRepresentationError::Unsupported(
            "shared-qualified values require UPC distributed-memory semantics".into(),
        ));
    }
    if let (Some(wrapper_size), Some(target_size)) = (wrapper.byte_size, target.byte_size)
        && wrapper_size != target_size
    {
        return Err(TransparentRepresentationError::Unsupported(
            format!(
                "transparent type wrapper size {wrapper_size} differs from target size {target_size}"
            )
            .into(),
        ));
    }
    Ok(())
}

/// Why a value shape could not be resolved from a type graph.
///
/// The variant distinguishes defective metadata (`Malformed`) from valid
/// metadata whose shape the debugger does not yet implement (`Unsupported`)
/// so callers can map each to the correct public state.
#[derive(Debug)]
pub(super) enum ValueShapeError {
    /// The type graph is defective: a wrapper cycle, an indirection with no
    /// byte size, or an underlying malformed/incomplete type entry.
    Malformed(Arc<str>),
    /// The type is valid but its value shape is not implemented.
    Unsupported(Arc<str>),
}

/// The widest indirection representation `decode_address` can turn into a
/// `VirtualAddress`.
pub(super) const MAX_ADDRESS_BYTES: u64 = 8;

/// Resolves the storage size of a pointer or reference value.
///
/// The type builder only leaves `byte_size` unset for a non-default address
/// class with no explicit `DW_AT_byte_size`, which is valid target-specific
/// metadata this backend cannot size rather than defective metadata. A missing
/// size under the default address class would be an internal inconsistency, so
/// the two cases are classified distinctly. A zero-byte indirection cannot hold
/// an address, so it is rejected as defective at this boundary rather than
/// permitting a zero-length read that would only fail later. A width wider than
/// a decodable address is valid-but-unsupported metadata and is rejected here so
/// inspection never performs a doomed inferior read.
pub(super) fn indirection_byte_size(
    byte_size: Option<u64>,
    address_class: u64,
    kind: &str,
) -> std::result::Result<u64, ValueShapeError> {
    match byte_size {
        Some(0) => Err(ValueShapeError::Malformed(
            format!("{kind} type has a zero byte size").into(),
        )),
        Some(byte_size) if byte_size > MAX_ADDRESS_BYTES => Err(ValueShapeError::Unsupported(
            format!(
                "{kind} type occupies {byte_size} bytes; addresses wider than \
                 {MAX_ADDRESS_BYTES} bytes are unsupported"
            )
            .into(),
        )),
        Some(byte_size) => Ok(byte_size),
        None if address_class != 0 => Err(ValueShapeError::Unsupported(
            format!("{kind} representation for address class {address_class} is unsupported")
                .into(),
        )),
        None => Err(ValueShapeError::Malformed(
            format!("{kind} type has no byte size").into(),
        )),
    }
}

pub(super) fn value_shape_from<T: TypeMetadataEntry>(
    types: &[T],
    id: TypeId,
) -> std::result::Result<ValueShape, ValueShapeError> {
    nested_value_shape(types, id, 0)
}

/// Computes a value shape for a type nested `depth` arrays deep. Deeper
/// nesting than the resolution limit is unsupported rather than recursed.
#[expect(
    clippy::too_many_lines,
    reason = "each normalized type shape has distinct validation"
)]
fn nested_value_shape<T: TypeMetadataEntry>(
    types: &[T],
    id: TypeId,
    depth: usize,
) -> std::result::Result<ValueShape, ValueShapeError> {
    if depth >= MAX_TYPE_RESOLUTION_DEPTH {
        return Err(ValueShapeError::Unsupported(
            "array nesting exceeds its limit".into(),
        ));
    }
    let mut current = id;
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(current) {
            return Err(ValueShapeError::Malformed("type wrapper cycle".into()));
        }
        let info = type_info_from(types, current).map_err(ValueShapeError::Malformed)?;
        match &info.kind {
            TypeKind::Base(base) => {
                if base.byte_size == 0 {
                    // A scalar encoding cannot occupy zero bytes; treat it as
                    // defective rather than decoding empty storage.
                    return Err(ValueShapeError::Malformed(
                        "base type has a zero byte size".into(),
                    ));
                }
                if base.byte_size > MAX_SCALAR_BYTES {
                    return Err(ValueShapeError::Unsupported(
                        format!("scalar type occupies {} bytes", base.byte_size).into(),
                    ));
                }
                let mut base = base.clone();
                base.name = Arc::clone(
                    &type_info_from(types, id)
                        .map_err(ValueShapeError::Malformed)?
                        .name,
                );
                return Ok(ValueShape {
                    kind: ValueShapeKind::Scalar(base),
                });
            }
            TypeKind::Enumeration {
                representation,
                enumerators,
                ..
            } => {
                if representation.byte_size == 0 {
                    return Err(ValueShapeError::Malformed(
                        "enumeration has a zero byte size".into(),
                    ));
                }
                if representation.byte_size > MAX_SCALAR_BYTES {
                    return Err(ValueShapeError::Unsupported(
                        format!("enumeration occupies {} bytes", representation.byte_size).into(),
                    ));
                }
                integer_bit_width(representation).map_err(ValueShapeError::Malformed)?;
                let mut representation = representation.clone();
                representation.name = Arc::clone(
                    &type_info_from(types, id)
                        .map_err(ValueShapeError::Malformed)?
                        .name,
                );
                return Ok(ValueShape {
                    kind: ValueShapeKind::Enumeration {
                        byte_size: representation.byte_size,
                        representation,
                        enumerators: Arc::clone(enumerators),
                    },
                });
            }
            TypeKind::Array {
                element,
                dimensions,
            } => {
                let element_shape = nested_value_shape(types, element.id, depth + 1)?;
                let mut count = 1_u64;
                for dimension in dimensions.iter() {
                    count = count.checked_mul(dimension.count).ok_or_else(|| {
                        ValueShapeError::Unsupported("array element count overflows".into())
                    })?;
                }
                let element_size = element_shape.byte_size();
                let byte_size = count.checked_mul(element_size).ok_or_else(|| {
                    ValueShapeError::Unsupported("array byte size overflows".into())
                })?;
                return Ok(ValueShape {
                    kind: ValueShapeKind::Array {
                        element: element.id,
                        dimensions: Arc::clone(dimensions),
                        byte_size,
                    },
                });
            }
            TypeKind::Slice {
                element,
                has_capacity,
                text,
            } => {
                let byte_size = info.byte_size.ok_or_else(|| {
                    ValueShapeError::Malformed("slice descriptor has no byte size".into())
                })?;
                return Ok(ValueShape {
                    kind: ValueShapeKind::Slice {
                        element: element.id,
                        byte_size,
                        has_capacity: *has_capacity,
                        text: *text,
                    },
                });
            }
            TypeKind::Record {
                members,
                bases,
                incomplete,
                ..
            } => {
                if *incomplete {
                    return Err(ValueShapeError::Unsupported(
                        "incomplete record values are unsupported".into(),
                    ));
                }
                let byte_size = info.byte_size.ok_or_else(|| {
                    ValueShapeError::Malformed("complete record type has no byte size".into())
                })?;
                return Ok(ValueShape {
                    kind: ValueShapeKind::Record {
                        record: current,
                        members: Arc::clone(members),
                        bases: Arc::clone(bases),
                        byte_size,
                    },
                });
            }
            TypeKind::Union {
                members,
                incomplete,
            } => {
                if *incomplete {
                    return Err(ValueShapeError::Unsupported(
                        "incomplete union values are unsupported".into(),
                    ));
                }
                let byte_size = info.byte_size.ok_or_else(|| {
                    ValueShapeError::Malformed("complete union type has no byte size".into())
                })?;
                return Ok(ValueShape {
                    kind: ValueShapeKind::Union {
                        union: current,
                        members: Arc::clone(members),
                        byte_size,
                    },
                });
            }
            TypeKind::Variant {
                common_members,
                bases,
                discriminant,
                variants,
                incomplete,
                ..
            } => {
                if *incomplete {
                    return Err(ValueShapeError::Unsupported(
                        "incomplete variant values are unsupported".into(),
                    ));
                }
                if matches!(discriminant.as_ref(), VariantDiscriminant::TagType(_))
                    && !is_single_default_variant(variants)
                {
                    return Err(ValueShapeError::Unsupported(
                        "tagless variant selection is unsupported".into(),
                    ));
                }
                let byte_size = info.byte_size.ok_or_else(|| {
                    ValueShapeError::Malformed("complete variant type has no byte size".into())
                })?;
                return Ok(ValueShape {
                    kind: ValueShapeKind::Variant {
                        aggregate: current,
                        common_members: Arc::clone(common_members),
                        bases: Arc::clone(bases),
                        discriminant: discriminant.as_ref().clone(),
                        variants: Arc::clone(variants),
                        byte_size,
                    },
                });
            }
            TypeKind::Pointer {
                target,
                address_class,
            } => {
                let byte_size = indirection_byte_size(info.byte_size, *address_class, "pointer")?;
                return Ok(ValueShape {
                    kind: ValueShapeKind::Indirection {
                        target: target.map(|target| target.id),
                        byte_size,
                        address_class: *address_class,
                    },
                });
            }
            TypeKind::Reference {
                target,
                address_class,
                ..
            } => {
                let byte_size = indirection_byte_size(info.byte_size, *address_class, "reference")?;
                return Ok(ValueShape {
                    kind: ValueShapeKind::Indirection {
                        target: Some(target.id),
                        byte_size,
                        address_class: *address_class,
                    },
                });
            }
            TypeKind::Modified { target, .. }
            | TypeKind::Named {
                target: Some(target),
                ..
            } => {
                transparent_representation(types, info, *target).map_err(|error| match error {
                    TransparentRepresentationError::Malformed(reason) => {
                        ValueShapeError::Malformed(reason)
                    }
                    TransparentRepresentationError::Unsupported(reason) => {
                        ValueShapeError::Unsupported(reason)
                    }
                })?;
                current = target.id;
            }
            TypeKind::Named { target: None, .. } => {
                return Err(ValueShapeError::Unsupported(
                    "incomplete named type has no representation target".into(),
                ));
            }
            TypeKind::Unspecified => {
                return Err(ValueShapeError::Unsupported(
                    "unspecified values are unsupported".into(),
                ));
            }
            TypeKind::Opaque { description } => {
                return Err(ValueShapeError::Unsupported(Arc::clone(description)));
            }
        }
    }
}

impl ValueShape {
    pub(super) const fn byte_size(&self) -> u64 {
        match &self.kind {
            ValueShapeKind::Scalar(base) => base.byte_size,
            ValueShapeKind::Enumeration { byte_size, .. }
            | ValueShapeKind::Indirection { byte_size, .. }
            | ValueShapeKind::Array { byte_size, .. }
            | ValueShapeKind::Slice { byte_size, .. }
            | ValueShapeKind::Record { byte_size, .. }
            | ValueShapeKind::Union { byte_size, .. }
            | ValueShapeKind::Variant { byte_size, .. } => *byte_size,
        }
    }

    pub(super) const fn scalar(&self) -> Option<&BaseType> {
        match &self.kind {
            ValueShapeKind::Scalar(base) => Some(base),
            ValueShapeKind::Enumeration { .. }
            | ValueShapeKind::Indirection { .. }
            | ValueShapeKind::Array { .. }
            | ValueShapeKind::Slice { .. }
            | ValueShapeKind::Record { .. }
            | ValueShapeKind::Union { .. }
            | ValueShapeKind::Variant { .. } => None,
        }
    }
}
