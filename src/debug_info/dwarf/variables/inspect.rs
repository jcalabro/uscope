//! Inspecting data objects: following value paths and materializing values.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use gimli::Location;

use crate::debug_info::dwarf::DwarfError;
use crate::debug_info::{
    ObjectStorage, PlannedStep, Step, StorageClass, VariableContext, VariableRuntime,
};
use crate::inspection::InspectionBudget;
use crate::model::{ArrayDimension, ValueStorage};
use crate::{
    AddressValue, BaseTypeEncoding, ByteOrder, CodeInstanceId, DereferenceReference,
    DereferenceState, DereferenceUnavailableReason, DereferencedValue, Error, ImageAddress,
    InspectedValue, IntegerValue, RecordMember, RecordMemberLayout, Result, TypeId, TypeInfo,
    TypeKind, ValueChild, ValueChildPage, ValueChildRelationship, ValueChildren,
    ValueChildrenReference, Variable, VariableInvalidReason, VariableMalformedKind, VariableState,
    VariableUnavailableReason, VariableValue, VariableValueSource, Variant, VariantDiscriminant,
    VirtualAddress,
};

use super::codec::{
    decode_address, decode_integer_value, decode_scalar, extract_bit_field, unsigned_value,
};
use super::evaluate::{
    EvaluateError, FrameBase, FrameBaseCache, FrameBaseContext, evaluate,
    evaluate_dynamic_aggregate_address, incomplete_piece_reason, materialize_constant,
    materialize_pieces,
};
use super::location::{Expression, ExpressionUse, LocationSelectionError};
use super::shape::{
    TransparentRepresentationError, ValueShape, ValueShapeError, ValueShapeKind,
    indirection_byte_size, transparent_representation, value_shape_from,
};
use super::types::{
    DynamicAggregateChild, DynamicAggregateLayoutKey, TypeResolution, type_info_from,
};
use super::variant::{is_single_default_variant, selected_variant_index};
use super::{
    CatalogDataObject, CatalogFunction, DwarfVariableInfo, MAX_AGGREGATE_DEPTH,
    MAX_EVALUATION_MEMORY_BYTES, MAX_LOCATION_PIECES, Metadata, MetadataAbsence, ValueDescription,
    malformed_reason,
};

/// One transition between storages, planned from types alone. Index steps
/// take their index values when they are applied.
#[derive(Clone)]
pub(in crate::debug_info) enum PathStep {
    Dereference {
        target: TypeId,
        byte_size: u64,
        address_class: u64,
    },
    ArrayIndex {
        dimensions: Arc<[ArrayDimension]>,
        element_size: u64,
    },
    SliceIndex {
        element_size: u64,
        descriptor_size: u64,
        has_capacity: bool,
    },
    Member(Box<PlannedMemberStep>),
    Unavailable(VariableUnavailableReason),
}

#[derive(Clone)]
pub(in crate::debug_info) struct PlannedMemberStep {
    pub(super) aggregate: TypeId,
    pub(super) child: DynamicAggregateChild,
    pub(super) member: RecordMember,
    pub(super) required_variant: Option<(usize, VariantDiscriminant, Arc<[Variant]>)>,
}

#[derive(Clone)]
pub(super) enum LocatedStorage {
    Memory(VirtualAddress),
    Bytes {
        source: VariableValueSource,
        raw: Arc<[u8]>,
        start: usize,
        end: usize,
        address: Option<VirtualAddress>,
    },
    ImplicitPointer {
        debug_info_offset: u64,
        byte_offset: i64,
    },
}

pub(super) struct DecodedSlice {
    pub(super) source: VariableValueSource,
    pub(super) raw: Arc<[u8]>,
    pub(super) address: VirtualAddress,
    pub(super) length: u64,
    pub(super) capacity: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ArrayIndexCalculationError {
    OutOfBounds {
        index: i128,
        lower_bound: i128,
        count: u64,
    },
    Overflow,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ScalarDecodeError {
    Unavailable(VariableUnavailableReason),
    Invalid(VariableInvalidReason),
    Malformed(Arc<str>),
}

pub(super) fn path_error_state(error: EvaluateError) -> Result<VariableState> {
    match error {
        EvaluateError::Unavailable(reason) => Ok(VariableState::Unavailable(reason)),
        EvaluateError::Malformed(description) => Ok(VariableState::Malformed(malformed_reason(
            VariableMalformedKind::InvalidExpression,
            description,
        ))),
        EvaluateError::Fatal(description) => Err(Error::VariableRuntime(description)),
    }
}

pub(super) fn row_major_array_index(
    dimensions: &[ArrayDimension],
    indices: &[i128],
) -> std::result::Result<u64, ArrayIndexCalculationError> {
    assert_eq!(
        dimensions.len(),
        indices.len(),
        "array index planning must supply one source index per dimension"
    );
    let mut linear = 0_u64;
    for (index, dimension) in indices.iter().copied().zip(dimensions) {
        let relative = index
            .checked_sub(dimension.lower_bound)
            .and_then(|relative| u64::try_from(relative).ok());
        let Some(relative) = relative.filter(|relative| *relative < dimension.count) else {
            return Err(ArrayIndexCalculationError::OutOfBounds {
                index,
                lower_bound: dimension.lower_bound,
                count: dimension.count,
            });
        };
        linear = linear
            .checked_mul(dimension.count)
            .and_then(|value| value.checked_add(relative))
            .ok_or(ArrayIndexCalculationError::Overflow)?;
    }
    Ok(linear)
}

/// The byte offset an array index step reaches with `indices`, checked
/// against the array's static bounds; `None` for any other step.
pub(in crate::debug_info) fn array_byte_offset(
    step: &PathStep,
    indices: &[i128],
) -> Result<Option<i64>> {
    let PathStep::ArrayIndex {
        dimensions,
        element_size,
    } = step
    else {
        return Ok(None);
    };
    if indices.len() != dimensions.len() {
        return Err(Error::InvalidValueExpression(format!(
            "an array of {} dimensions takes as many indices, not {}",
            dimensions.len(),
            indices.len()
        )));
    }
    let linear = row_major_array_index(dimensions, indices).map_err(|error| match error {
        ArrayIndexCalculationError::OutOfBounds {
            index,
            lower_bound,
            count,
        } => Error::ValueIndexOutOfBounds {
            index,
            lower_bound,
            count,
        },
        ArrayIndexCalculationError::Overflow => {
            Error::InvalidValueExpression("array row-major index overflows".to_owned())
        }
    })?;
    linear
        .checked_mul(*element_size)
        .and_then(|offset| i64::try_from(offset).ok())
        .map(Some)
        .ok_or_else(|| Error::InvalidValueExpression("array element offset overflows".to_owned()))
}

pub(super) fn static_member_layout_is_valid(
    record_size: Option<u64>,
    member_size: Option<u64>,
    layout: RecordMemberLayout,
) -> bool {
    match layout {
        RecordMemberLayout::ByteOffset(offset) => {
            let Some(record_size) = record_size else {
                return false;
            };
            offset <= record_size
                && member_size.is_none_or(|size| {
                    offset
                        .checked_add(size)
                        .is_some_and(|end| end <= record_size)
                })
        }
        RecordMemberLayout::BitRange {
            bit_offset,
            bit_size,
        } => record_size.is_some_and(|record_size| {
            record_size.checked_mul(8).is_some_and(|record_bits| {
                bit_offset
                    .checked_add(bit_size)
                    .is_some_and(|end| end <= record_bits)
            })
        }),
        RecordMemberLayout::Runtime => true,
    }
}

impl DwarfVariableInfo {
    /// Classifies an object's storage from the operations of its location
    /// expressions. Thread-local and indirect forms dominate frame-relative
    /// ones, which dominate static addresses.
    pub(super) fn object_storage(&self, object: &CatalogDataObject) -> ObjectStorage {
        let ranges = Arc::clone(&object.ranges);
        let Metadata::Value(ValueDescription::Location(location)) = &object.value else {
            return ObjectStorage {
                class: StorageClass::NotMemory,
                ranges,
            };
        };
        let mut uses = BTreeSet::new();
        for entry in location.entries.iter() {
            if self.expression_uses(&entry.expression, &mut uses).is_err() {
                return ObjectStorage {
                    class: StorageClass::NotMemory,
                    ranges,
                };
            }
        }
        let class = if uses.contains(&ExpressionUse::ThreadLocal) {
            StorageClass::ThreadLocal
        } else if uses.contains(&ExpressionUse::Dereference) {
            StorageClass::Indirect
        } else if uses.contains(&ExpressionUse::RegisterValue)
            || uses.contains(&ExpressionUse::Computed)
            || location.entries.is_empty()
        {
            StorageClass::NotMemory
        } else if uses.contains(&ExpressionUse::Frame) {
            let single_location = location
                .entries
                .iter()
                .all(|entry| entry.expression.bytes == location.entries[0].expression.bytes);
            let single_frame_base = match &object.frame_base {
                Metadata::Value(frame_base) => frame_base
                    .entries
                    .iter()
                    .all(|entry| entry.expression.bytes == frame_base.entries[0].expression.bytes),
                Metadata::Absent(_) => true,
                Metadata::Malformed(_) => false,
            };
            StorageClass::Frame {
                stable: single_location && single_frame_base,
                moving_stack: location.entries.iter().any(|entry| {
                    self.evaluation_units
                        .get(entry.expression.unit)
                        .and_then(|unit| unit.language)
                        == Some(gimli::DW_LANG_Go)
                }),
            }
        } else {
            StorageClass::Static
        };
        ObjectStorage { class, ranges }
    }

    pub(super) fn expression_uses(
        &self,
        expression: &Expression,
        uses: &mut BTreeSet<ExpressionUse>,
    ) -> std::result::Result<(), gimli::Error> {
        let reader = gimli::EndianSlice::new(&expression.bytes, self.endian);
        let mut operations = gimli::Expression(reader).operations(expression.encoding);
        while let Some(operation) = operations.next()? {
            uses.extend(match operation {
                gimli::Operation::TLS => Some(ExpressionUse::ThreadLocal),
                gimli::Operation::Deref { .. } => Some(ExpressionUse::Dereference),
                gimli::Operation::FrameOffset { .. }
                | gimli::Operation::RegisterOffset { .. }
                | gimli::Operation::CallFrameCFA => Some(ExpressionUse::Frame),
                gimli::Operation::Register { .. } => Some(ExpressionUse::RegisterValue),
                gimli::Operation::StackValue
                | gimli::Operation::ImplicitValue { .. }
                | gimli::Operation::ImplicitPointer { .. }
                | gimli::Operation::Piece { .. }
                | gimli::Operation::EntryValue { .. } => Some(ExpressionUse::Computed),
                _ => None,
            });
        }
        Ok(())
    }

    pub(super) fn type_info(&self, id: TypeId) -> std::result::Result<&TypeInfo, Arc<str>> {
        type_info_from(&self.types, id)
    }

    pub(super) fn value_shape(
        &self,
        id: TypeId,
    ) -> std::result::Result<ValueShape, ValueShapeError> {
        value_shape_from(&self.types, id)
    }

    pub(super) fn function_at(&self, address: ImageAddress) -> Option<&CatalogFunction> {
        self.address_index
            .range(..=address)
            .rev()
            .flat_map(|(_, functions)| functions.iter().copied())
            .find_map(|index| {
                let function = &self.functions[index];
                function
                    .ranges
                    .iter()
                    .any(|range| range.contains(address))
                    .then_some(function)
            })
    }

    pub(super) fn transparent_type(
        &self,
        id: TypeId,
    ) -> std::result::Result<(TypeId, &TypeInfo), ValueShapeError> {
        let mut current = id;
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(current) {
                return Err(ValueShapeError::Malformed("type wrapper cycle".into()));
            }
            let info = self
                .type_info(current)
                .map_err(ValueShapeError::Malformed)?;
            match info.kind {
                TypeKind::Modified { target, .. }
                | TypeKind::Named {
                    target: Some(target),
                    ..
                } => {
                    transparent_representation(&self.types, info, target).map_err(|error| {
                        match error {
                            TransparentRepresentationError::Malformed(reason) => {
                                ValueShapeError::Malformed(reason)
                            }
                            TransparentRepresentationError::Unsupported(reason) => {
                                ValueShapeError::Unsupported(reason)
                            }
                        }
                    })?;
                    current = target.id;
                }
                TypeKind::Named { target: None, .. } => {
                    return Err(ValueShapeError::Unsupported(
                        "incomplete named type has no representation target".into(),
                    ));
                }
                _ => return Ok((current, info)),
            }
        }
    }

    pub(super) fn validate_static_member_layout(
        &self,
        record: TypeId,
        member: &RecordMember,
    ) -> Result<()> {
        let record_size = self.type_info(record).ok().and_then(|info| info.byte_size);
        let member_size = self
            .type_info(member.type_ref.id)
            .ok()
            .and_then(|info| info.byte_size);
        if !static_member_layout_is_valid(record_size, member_size, member.layout) {
            return Err(Error::debug_info(DwarfError::MalformedVariable(
                "record member extends beyond its containing object".into(),
            )));
        }
        Ok(())
    }

    /// Plans one structural step from `from`, which reads no program
    /// state. A member step follows pointers to the record that holds the
    /// member; an index step takes one index for a slice and one per
    /// dimension for an array, of the `available` the caller holds.
    #[expect(
        clippy::too_many_lines,
        reason = "each step keeps the typed failure of every type shape it meets"
    )]
    pub(super) fn plan_step(&self, from: TypeId, step: Step<'_>) -> Result<PlannedStep> {
        enum AggregateMembers<'a> {
            Direct(&'a [RecordMember]),
            Variant {
                common_members: &'a [RecordMember],
                discriminant: &'a VariantDiscriminant,
                variants: &'a Arc<[Variant]>,
            },
        }

        let malformed = |description| Error::debug_info(DwarfError::MalformedVariable(description));
        let planned = |steps, consumed, result| PlannedStep {
            steps,
            consumed,
            result,
        };
        let unsupported =
            || PathStep::Unavailable(crate::UnsupportedVariableFeature::TypeRepresentation.into());
        let mut steps = Vec::new();
        match step {
            Step::Deref => {
                let (_canonical, info) = match self.transparent_type(from) {
                    Ok(value) => value,
                    Err(ValueShapeError::Malformed(description)) => {
                        return Err(malformed(description));
                    }
                    Err(ValueShapeError::Unsupported(_)) => {
                        return Ok(planned(vec![unsupported()], 0, None));
                    }
                };
                let (target, address_class) = match &info.kind {
                    TypeKind::Pointer {
                        target: Some(target),
                        address_class,
                    }
                    | TypeKind::Reference {
                        target,
                        address_class,
                        ..
                    } => (target.id, *address_class),
                    TypeKind::Pointer { target: None, .. } => {
                        let reason = VariableUnavailableReason::ValueAccess(
                            crate::ValueAccessUnavailableReason::UnspecifiedPointee,
                        );
                        return Ok(planned(vec![PathStep::Unavailable(reason)], 0, None));
                    }
                    _ => {
                        let reason = VariableUnavailableReason::ValueAccess(
                            crate::ValueAccessUnavailableReason::NotPointerOrReference,
                        );
                        return Ok(planned(vec![PathStep::Unavailable(reason)], 0, None));
                    }
                };
                match indirection_byte_size(info.byte_size, address_class, "pointer or reference") {
                    Ok(byte_size) => steps.push(PathStep::Dereference {
                        target,
                        byte_size,
                        address_class,
                    }),
                    Err(ValueShapeError::Malformed(description)) => {
                        return Err(malformed(description));
                    }
                    Err(ValueShapeError::Unsupported(_)) => steps.push(unsupported()),
                }
                Ok(planned(steps, 0, Some(target)))
            }
            Step::Index { available } => {
                let source_info = self.type_info(from).map_err(malformed)?;
                let (_canonical, info) = match self.transparent_type(from) {
                    Ok(value) => value,
                    Err(ValueShapeError::Malformed(description)) => {
                        return Err(malformed(description));
                    }
                    Err(ValueShapeError::Unsupported(_)) => {
                        return Ok(planned(vec![unsupported()], 1, None));
                    }
                };
                let (element, consumed) = match &info.kind {
                    TypeKind::Array {
                        element,
                        dimensions,
                    } => {
                        if available < dimensions.len() {
                            return Err(Error::IncompleteArrayIndex {
                                type_name: Arc::clone(&source_info.name),
                                expected: dimensions.len(),
                                supplied: available,
                            });
                        }
                        (element.id, dimensions.len())
                    }
                    TypeKind::Slice { element, .. } => (element.id, 1),
                    _ => {
                        return Err(Error::IndexAccessOnNonIndexable {
                            type_name: Arc::clone(&source_info.name),
                        });
                    }
                };
                let element_size = match self.value_shape(element) {
                    Ok(shape) => shape.byte_size(),
                    Err(ValueShapeError::Malformed(description)) => {
                        return Err(malformed(description));
                    }
                    Err(ValueShapeError::Unsupported(_)) => {
                        return Ok(planned(vec![unsupported()], consumed, Some(element)));
                    }
                };
                match &info.kind {
                    TypeKind::Array { dimensions, .. } => steps.push(PathStep::ArrayIndex {
                        dimensions: Arc::clone(dimensions),
                        element_size,
                    }),
                    TypeKind::Slice { has_capacity, .. } => {
                        let descriptor_size = info
                            .byte_size
                            .ok_or_else(|| malformed("slice descriptor has no byte size".into()))?;
                        steps.push(PathStep::SliceIndex {
                            element_size,
                            descriptor_size,
                            has_capacity: *has_capacity,
                        });
                    }
                    _ => unreachable!("only arrays and slices were accepted above"),
                }
                Ok(planned(steps, consumed, Some(element)))
            }
            Step::Member(member_name) => {
                let mut current = from;
                let mut indirections = HashSet::new();
                let (aggregate, aggregate_members) = loop {
                    let source_info = self.type_info(current).map_err(malformed)?;
                    let (canonical, info) = match self.transparent_type(current) {
                        Ok(value) => value,
                        Err(ValueShapeError::Malformed(description)) => {
                            return Err(malformed(description));
                        }
                        Err(ValueShapeError::Unsupported(_)) => {
                            steps.push(unsupported());
                            return Ok(planned(steps, 0, None));
                        }
                    };
                    match &info.kind {
                        TypeKind::Pointer {
                            target: Some(target),
                            address_class,
                        }
                        | TypeKind::Reference {
                            target,
                            address_class,
                            ..
                        } => {
                            if !indirections.insert(canonical) || steps.len() >= MAX_AGGREGATE_DEPTH
                            {
                                return Err(Error::InvalidValueExpression(
                                    "pointer traversal exceeds its limit or contains a cycle"
                                        .to_owned(),
                                ));
                            }
                            match indirection_byte_size(
                                info.byte_size,
                                *address_class,
                                "pointer or reference",
                            ) {
                                Ok(byte_size) => steps.push(PathStep::Dereference {
                                    target: target.id,
                                    byte_size,
                                    address_class: *address_class,
                                }),
                                Err(ValueShapeError::Malformed(description)) => {
                                    return Err(malformed(description));
                                }
                                Err(ValueShapeError::Unsupported(_)) => steps.push(unsupported()),
                            }
                            current = target.id;
                        }
                        TypeKind::Record { members, .. } | TypeKind::Union { members, .. } => {
                            break (canonical, AggregateMembers::Direct(members));
                        }
                        TypeKind::Variant {
                            common_members,
                            discriminant,
                            variants,
                            ..
                        } => {
                            break (
                                canonical,
                                AggregateMembers::Variant {
                                    common_members,
                                    discriminant: discriminant.as_ref(),
                                    variants,
                                },
                            );
                        }
                        _ => {
                            return Err(Error::MemberAccessOnNonRecord {
                                member: member_name.to_owned(),
                                type_name: Arc::clone(&source_info.name),
                            });
                        }
                    }
                };
                let named = |member: &&RecordMember| {
                    !member.artificial && member.name.as_deref() == Some(member_name)
                };
                let mut matching = Vec::new();
                match aggregate_members {
                    AggregateMembers::Direct(members) => {
                        matching.extend(
                            members
                                .iter()
                                .enumerate()
                                .filter(|(_, member)| named(member))
                                .map(|(index, member)| {
                                    (DynamicAggregateChild::Member(index), None, member)
                                }),
                        );
                    }
                    AggregateMembers::Variant {
                        common_members,
                        discriminant,
                        variants,
                    } => {
                        matching.extend(
                            common_members
                                .iter()
                                .enumerate()
                                .filter(|(_, member)| named(member))
                                .map(|(index, member)| {
                                    (DynamicAggregateChild::Member(index), None, member)
                                }),
                        );
                        for (variant_index, variant) in variants.iter().enumerate() {
                            matching.extend(
                                variant
                                    .members
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, member)| named(member))
                                    .map(|(member_index, member)| {
                                        (
                                            DynamicAggregateChild::VariantMember {
                                                variant: variant_index,
                                                member: member_index,
                                            },
                                            Some((
                                                variant_index,
                                                discriminant.clone(),
                                                Arc::clone(variants),
                                            )),
                                            member,
                                        )
                                    }),
                            );
                        }
                    }
                }
                let [(child, required_variant, member)] = matching.as_slice() else {
                    let type_name = Arc::clone(
                        &self
                            .type_info(aggregate)
                            .expect("aggregate type resolved")
                            .name,
                    );
                    if matching.is_empty() {
                        return Err(Error::MemberNotFound {
                            member: member_name.to_owned(),
                            type_name,
                        });
                    }
                    return Err(Error::AmbiguousMember {
                        member: member_name.to_owned(),
                        type_name,
                    });
                };
                self.validate_static_member_layout(aggregate, member)?;
                steps.push(PathStep::Member(Box::new(PlannedMemberStep {
                    aggregate,
                    child: *child,
                    member: (*member).clone(),
                    required_variant: required_variant.clone(),
                })));
                Ok(planned(steps, 0, Some(member.type_ref.id)))
            }
        }
    }

    /// The index of the data object `name` names in the selected logical
    /// frame, whose innermost declaration hides the others.
    pub(super) fn visible_object(
        &self,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        name: &str,
    ) -> Result<usize> {
        let function = self
            .function_at(address)
            .ok_or_else(|| Error::VariableNotFound(name.to_owned()))?;
        let mut named = function
            .objects
            .iter()
            .copied()
            .filter(|&index| {
                let object = &self.objects[index];
                object.instance == selected
                    && object.ranges.iter().any(|range| range.contains(address))
                    && object.name.as_ref() == name
            })
            .collect::<Vec<_>>();
        let depth = named
            .iter()
            .map(|&index| self.objects[index].lexical_depth)
            .max()
            .ok_or_else(|| Error::VariableNotFound(name.to_owned()))?;
        named.retain(|&index| self.objects[index].lexical_depth == depth);
        let [index] = named.as_slice() else {
            return Err(Error::AmbiguousVariable(name.to_owned()));
        };
        Ok(*index)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "location selection preserves each DWARF storage form and its typed failure"
    )]
    pub(super) fn located_data_object(
        &self,
        variable: &CatalogDataObject,
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        frame_base_cache: &mut FrameBaseCache,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<LocatedStorage, EvaluateError> {
        if let Some(description) = &variable.malformed {
            return Err(EvaluateError::Malformed(Arc::clone(description)));
        }
        let type_id = match &variable.type_info {
            TypeResolution::Resolved(id) => *id,
            TypeResolution::Malformed(description) => {
                return Err(EvaluateError::Malformed(Arc::clone(description)));
            }
        };
        let shape = self.value_shape(type_id).map_err(EvaluateError::from)?;
        let description = match &variable.value {
            Metadata::Value(description) => description,
            Metadata::Absent(MetadataAbsence::NoLocation) => {
                return Err(EvaluateError::Unavailable(
                    VariableUnavailableReason::OptimizedOut(crate::OptimizedOutReason::NoLocation),
                ));
            }
            Metadata::Absent(MetadataAbsence::NoFrameBase | MetadataAbsence::NotApplicable) => {
                unreachable!("data-object value cannot contain a frame-base absence")
            }
            Metadata::Malformed(description) => {
                return Err(EvaluateError::Malformed(Arc::clone(description)));
            }
        };
        if let ValueDescription::Constant(constant) = description {
            let raw = materialize_constant(
                constant,
                usize::try_from(shape.byte_size())
                    .map_err(|_| VariableUnavailableReason::EvaluationLimit)?,
                self.target,
            )?;
            let end = raw.len();
            return Ok(LocatedStorage::Bytes {
                source: VariableValueSource::Constant,
                raw,
                start: 0,
                end,
                address: None,
            });
        }
        let ValueDescription::Location(location) = description else {
            unreachable!("constant values returned above")
        };
        let expression = location
            .expression(address)
            .map_err(|error| match error {
                LocationSelectionError::Unavailable(reason) => EvaluateError::Unavailable(reason),
                LocationSelectionError::Malformed(description) => {
                    EvaluateError::Malformed(description)
                }
            })?
            .ok_or({
                EvaluateError::Unavailable(VariableUnavailableReason::UnavailableAtInstruction)
            })?;
        let mut frame_base = FrameBase::Lazy(FrameBaseContext {
            location: &variable.frame_base,
            address,
            cache: frame_base_cache,
        });
        let pieces = evaluate(
            expression,
            self.endian,
            &mut frame_base,
            &self.evaluation_units,
            runtime,
            budget,
        )?;
        if pieces.len() > MAX_LOCATION_PIECES {
            return Err(VariableUnavailableReason::EvaluationLimit.into());
        }
        let expected_bits = shape
            .byte_size()
            .checked_mul(8)
            .ok_or(VariableUnavailableReason::EvaluationLimit)?;
        if let Some(reason) = incomplete_piece_reason(&pieces, expected_bits)? {
            return Err(reason.into());
        }
        let [piece] = pieces.as_slice() else {
            return Err(crate::UnsupportedVariableFeature::CompositeLocation.into());
        };
        if piece.size_in_bits.is_some_and(|size| size != expected_bits)
            || piece.bit_offset.is_some()
        {
            return Err(crate::UnsupportedVariableFeature::CompositeLocation.into());
        }
        match piece.location {
            Location::Address { address } => {
                Ok(LocatedStorage::Memory(VirtualAddress::new(address)))
            }
            Location::ImplicitPointer { value, byte_offset } => {
                Ok(LocatedStorage::ImplicitPointer {
                    debug_info_offset: u64::try_from(value.0)
                        .map_err(|_| VariableUnavailableReason::EvaluationLimit)?,
                    byte_offset,
                })
            }
            _ => {
                let (source, raw) = materialize_pieces(
                    &pieces,
                    shape.byte_size(),
                    shape.scalar(),
                    self.endian,
                    self.target,
                    runtime,
                    budget,
                )?;
                let end = raw.len();
                Ok(LocatedStorage::Bytes {
                    source,
                    raw,
                    start: 0,
                    end,
                    address: None,
                })
            }
        }
    }

    pub(super) fn storage_with_offset(
        storage: LocatedStorage,
        offset: i64,
    ) -> std::result::Result<LocatedStorage, EvaluateError> {
        match storage {
            LocatedStorage::Memory(address) => {
                let value = if offset >= 0 {
                    address.get().checked_add(offset.unsigned_abs())
                } else {
                    address.get().checked_sub(offset.unsigned_abs())
                }
                .ok_or({
                    EvaluateError::Unavailable(VariableUnavailableReason::ValueAccess(
                        crate::ValueAccessUnavailableReason::AddressOverflow,
                    ))
                })?;
                Ok(LocatedStorage::Memory(VirtualAddress::new(value)))
            }
            LocatedStorage::Bytes {
                source,
                raw,
                start,
                end,
                address,
            } => {
                let adjusted = if offset >= 0 {
                    start.checked_add(
                        usize::try_from(offset.unsigned_abs())
                            .map_err(|_| VariableUnavailableReason::EvaluationLimit)?,
                    )
                } else {
                    start.checked_sub(
                        usize::try_from(offset.unsigned_abs())
                            .map_err(|_| VariableUnavailableReason::EvaluationLimit)?,
                    )
                }
                .filter(|adjusted| *adjusted <= end)
                .ok_or_else(|| {
                    EvaluateError::Malformed("member offset is outside its containing value".into())
                })?;
                let address = address.and_then(|address| {
                    if offset >= 0 {
                        address.get().checked_add(offset.unsigned_abs())
                    } else {
                        address.get().checked_sub(offset.unsigned_abs())
                    }
                    .map(VirtualAddress::new)
                });
                Ok(LocatedStorage::Bytes {
                    source,
                    raw,
                    start: adjusted,
                    end,
                    address,
                })
            }
            LocatedStorage::ImplicitPointer { .. } => Err(EvaluateError::Malformed(
                "an unresolved implicit pointer cannot be offset".into(),
            )),
        }
    }

    pub(super) fn read_storage(
        storage: &LocatedStorage,
        size: usize,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<(VariableValueSource, Arc<[u8]>), EvaluateError> {
        match storage {
            LocatedStorage::Memory(address) => {
                budget.consume_memory(size)?;
                let raw = runtime.read_memory(*address, size)?;
                Ok((VariableValueSource::Memory(*address), raw))
            }
            LocatedStorage::Bytes {
                raw, start, end, ..
            } => {
                let selected_end = start
                    .checked_add(size)
                    .filter(|selected_end| *selected_end <= *end)
                    .ok_or_else(|| {
                        EvaluateError::Malformed(
                            "selected value extends beyond its containing storage".into(),
                        )
                    })?;
                Ok((
                    Self::storage_source(storage),
                    Arc::from(&raw[*start..selected_end]),
                ))
            }
            LocatedStorage::ImplicitPointer { .. } => Err(EvaluateError::Malformed(
                "an unresolved implicit pointer cannot be read".into(),
            )),
        }
    }

    pub(super) fn decode_slice(
        &self,
        storage: &LocatedStorage,
        byte_size: u64,
        has_capacity: bool,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<DecodedSlice, EvaluateError> {
        let pointer_bytes = match self.target.pointer_width {
            crate::PointerWidth::Bits32 => 4,
            crate::PointerWidth::Bits64 => 8,
        };
        let words = if has_capacity { 3 } else { 2 };
        // Validate the metadata's size before reading, so a bogus size cannot
        // spend the request's memory budget.
        let size = pointer_bytes * words;
        if byte_size != size as u64 {
            return Err(EvaluateError::Malformed(
                "slice descriptor size does not match its target layout".into(),
            ));
        }
        let (source, raw) = Self::read_storage(storage, size, runtime, budget)?;
        let word = |index: usize| {
            unsigned_value(
                &raw[index * pointer_bytes..(index + 1) * pointer_bytes],
                self.target.byte_order,
            )
            .map_err(|reason| Arc::<str>::from(reason.to_string()))
            .and_then(|value| {
                u64::try_from(value)
                    .map_err(|_| Arc::<str>::from("slice word exceeds target address width"))
            })
        };
        let address = word(0)
            .map(VirtualAddress::new)
            .map_err(EvaluateError::Malformed)?;
        let length = word(1).map_err(EvaluateError::Malformed)?;
        let capacity = if has_capacity {
            let capacity = word(2).map_err(EvaluateError::Malformed)?;
            if capacity < length {
                return Err(EvaluateError::Malformed(
                    "slice length exceeds its capacity".into(),
                ));
            }
            Some(capacity)
        } else {
            None
        };
        if address.get() == 0 && length != 0 {
            return Err(EvaluateError::Malformed(
                "non-empty slice has a null data pointer".into(),
            ));
        }
        Ok(DecodedSlice {
            source,
            raw,
            address,
            length,
            capacity,
        })
    }

    pub(super) const fn concrete_storage_address(
        storage: &LocatedStorage,
    ) -> Option<VirtualAddress> {
        match storage {
            LocatedStorage::Memory(address) => Some(*address),
            LocatedStorage::Bytes { address, .. } => *address,
            LocatedStorage::ImplicitPointer { .. } => None,
        }
    }

    pub(super) fn retained_storage(storage: &LocatedStorage) -> ValueStorage {
        match storage {
            LocatedStorage::Memory(address) => ValueStorage::Memory(*address),
            LocatedStorage::Bytes {
                source,
                raw,
                start,
                end,
                address,
            } => ValueStorage::Bytes {
                source: source.clone(),
                raw: Arc::clone(raw),
                start: *start,
                end: *end,
                address: *address,
            },
            LocatedStorage::ImplicitPointer {
                debug_info_offset,
                byte_offset,
            } => ValueStorage::ImplicitPointer {
                debug_info_offset: *debug_info_offset,
                byte_offset: *byte_offset,
            },
        }
    }

    pub(super) fn restored_storage(storage: &ValueStorage) -> LocatedStorage {
        match storage {
            ValueStorage::Memory(address) => LocatedStorage::Memory(*address),
            ValueStorage::Bytes {
                source,
                raw,
                start,
                end,
                address,
            } => LocatedStorage::Bytes {
                source: source.clone(),
                raw: Arc::clone(raw),
                start: *start,
                end: *end,
                address: *address,
            },
            ValueStorage::ImplicitPointer {
                debug_info_offset,
                byte_offset,
            } => LocatedStorage::ImplicitPointer {
                debug_info_offset: *debug_info_offset,
                byte_offset: *byte_offset,
            },
        }
    }

    /// Describes where a storage's bytes came from. Bytes read once for a
    /// whole page of elements still report each element's own address.
    pub(super) fn storage_source(storage: &LocatedStorage) -> VariableValueSource {
        match storage {
            LocatedStorage::Memory(address) => VariableValueSource::Memory(*address),
            LocatedStorage::Bytes {
                source, address, ..
            } => address.map_or_else(|| source.clone(), VariableValueSource::Memory),
            LocatedStorage::ImplicitPointer { .. } => VariableValueSource::ImplicitPointer,
        }
    }

    pub(super) fn child_reference(
        storage: &LocatedStorage,
        context: VariableContext,
        target_type: TypeId,
        total: u64,
        active_variant: Option<usize>,
    ) -> Arc<ValueChildrenReference> {
        Arc::new(ValueChildrenReference {
            stop_id: context.stop_id,
            thread: context.thread,
            frame: context.frame,
            module: context.module,
            image: context.image,
            context_address: context.address,
            target_type,
            storage: Self::retained_storage(storage),
            total,
            active_variant,
        })
    }

    pub(super) fn bit_field_storage(
        &self,
        storage: LocatedStorage,
        type_id: TypeId,
        bit_offset: u64,
        bit_size: u64,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<LocatedStorage, EvaluateError> {
        let shape = self.value_shape(type_id).map_err(EvaluateError::from)?;
        let base = match &shape.kind {
            ValueShapeKind::Scalar(base) => base,
            ValueShapeKind::Enumeration { representation, .. } => representation,
            _ => {
                return Err(EvaluateError::Unavailable(
                    VariableUnavailableReason::ValueAccess(
                        crate::ValueAccessUnavailableReason::NonIntegralBitField,
                    ),
                ));
            }
        };
        let storage_bits = base
            .byte_size
            .checked_mul(8)
            .ok_or(VariableUnavailableReason::EvaluationLimit)?;
        if bit_size == 0 || bit_size > storage_bits || bit_size > 128 {
            return Err(EvaluateError::Malformed(
                "bit-field width exceeds its declared scalar storage".into(),
            ));
        }
        let first_byte = bit_offset / 8;
        let last_bit = bit_offset
            .checked_add(bit_size)
            .ok_or(VariableUnavailableReason::EvaluationLimit)?;
        let last_byte = last_bit
            .checked_add(7)
            .ok_or(VariableUnavailableReason::EvaluationLimit)?
            / 8;
        let span = last_byte
            .checked_sub(first_byte)
            .and_then(|size| usize::try_from(size).ok())
            .ok_or(VariableUnavailableReason::EvaluationLimit)?;
        let selected = Self::storage_with_offset(
            storage,
            i64::try_from(first_byte).map_err(|_| VariableUnavailableReason::EvaluationLimit)?,
        )?;
        let (_, bytes) = Self::read_storage(&selected, span, runtime, budget)?;
        let relative_offset = bit_offset % 8;
        let mut value =
            extract_bit_field(&bytes, relative_offset, bit_size, self.target.byte_order)
                .map_err(EvaluateError::Malformed)?;
        if matches!(
            base.encoding,
            BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter
        ) && bit_size < 128
            && value & (1_u128 << (bit_size - 1)) != 0
        {
            value |= u128::MAX << bit_size;
        }
        let byte_size = usize::try_from(base.byte_size)
            .map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
        let full = value.to_le_bytes();
        let mut raw = full[..byte_size].to_vec();
        if self.target.byte_order == ByteOrder::Big {
            raw.reverse();
        }
        let raw: Arc<[u8]> = raw.into();
        let end = raw.len();
        Ok(LocatedStorage::Bytes {
            source: VariableValueSource::Computed,
            raw,
            start: 0,
            end,
            address: None,
        })
    }

    pub(super) fn runtime_member_storage(
        &self,
        storage: &LocatedStorage,
        aggregate: TypeId,
        child: DynamicAggregateChild,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<LocatedStorage, EvaluateError> {
        let object_address = Self::concrete_storage_address(storage).ok_or({
            EvaluateError::Unavailable(VariableUnavailableReason::ValueAccess(
                crate::ValueAccessUnavailableReason::NoConcreteObjectAddress,
            ))
        })?;
        let key = DynamicAggregateLayoutKey { aggregate, child };
        let address = evaluate_dynamic_aggregate_address(
            &self.dynamic_record_layouts,
            key,
            self.endian,
            &self.evaluation_units,
            runtime,
            budget,
            object_address,
        )?;
        Ok(LocatedStorage::Memory(address))
    }

    pub(super) fn active_variant_from_storage(
        &self,
        storage: &LocatedStorage,
        aggregate: TypeId,
        discriminant: &VariantDiscriminant,
        variants: &[Variant],
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<Option<usize>, EvaluateError> {
        self.variant_selection_from_storage(
            storage,
            aggregate,
            discriminant,
            variants,
            runtime,
            budget,
        )
        .map(|(_, active)| active)
    }

    pub(super) fn variant_selection_from_storage(
        &self,
        storage: &LocatedStorage,
        aggregate: TypeId,
        discriminant: &VariantDiscriminant,
        variants: &[Variant],
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<(Option<IntegerValue>, Option<usize>), EvaluateError> {
        let VariantDiscriminant::Stored(member) = discriminant else {
            // Without stored discriminator bytes, only a lone default variant
            // is known to be active; choosing among several would be a guess.
            return if is_single_default_variant(variants) {
                Ok((None, Some(0)))
            } else {
                Err(EvaluateError::Unavailable(
                    crate::UnsupportedVariableFeature::TypeRepresentation.into(),
                ))
            };
        };
        let shape = self.value_shape(member.type_ref.id)?;
        let representation = match &shape.kind {
            ValueShapeKind::Scalar(base)
                if !matches!(base.encoding, BaseTypeEncoding::Floating) =>
            {
                base
            }
            ValueShapeKind::Enumeration { representation, .. } => representation,
            _ => {
                return Err(EvaluateError::Malformed(
                    "variant discriminator type is not integral".into(),
                ));
            }
        };
        let selected = match member.layout {
            RecordMemberLayout::ByteOffset(offset) => Self::storage_with_offset(
                storage.clone(),
                i64::try_from(offset).map_err(|_| VariableUnavailableReason::EvaluationLimit)?,
            )?,
            RecordMemberLayout::BitRange {
                bit_offset,
                bit_size,
            } => self.bit_field_storage(
                storage.clone(),
                member.type_ref.id,
                bit_offset,
                bit_size,
                runtime,
                budget,
            )?,
            RecordMemberLayout::Runtime => self.runtime_member_storage(
                storage,
                aggregate,
                DynamicAggregateChild::Discriminant,
                runtime,
                budget,
            )?,
        };
        let size = usize::try_from(representation.byte_size)
            .map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
        let (_, raw) = Self::read_storage(&selected, size, runtime, budget)?;
        let value = decode_integer_value(representation, &raw, self.target.byte_order)
            .map_err(EvaluateError::Malformed)?;
        let active = selected_variant_index(variants, value).map_err(EvaluateError::Malformed)?;
        Ok((Some(value), active))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "implicit-pointer resolution requires target bounds and the shared evaluation context"
    )]
    pub(super) fn resolve_implicit_pointer(
        &self,
        debug_info_offset: u64,
        byte_offset: i64,
        target: TypeId,
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        frame_base_cache: &mut FrameBaseCache,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<LocatedStorage, EvaluateError> {
        let object_index = self
            .objects_by_debug_offset
            .get(&debug_info_offset)
            .copied()
            .ok_or_else(|| {
                EvaluateError::Unavailable(
                    crate::UnsupportedVariableFeature::CrossDieEvaluation.into(),
                )
            })?;
        let object = &self.objects[object_index];
        let referenced_type = match &object.type_info {
            TypeResolution::Resolved(id) => *id,
            TypeResolution::Malformed(description) => {
                return Err(EvaluateError::Malformed(Arc::clone(description)));
            }
        };
        let referenced_size = self.value_shape(referenced_type)?.byte_size();
        let target_size = self.value_shape(target)?.byte_size();
        implicit_pointer_range(byte_offset, target_size, referenced_size)
            .map_err(EvaluateError::Unavailable)?;
        let referenced =
            self.located_data_object(object, address, runtime, frame_base_cache, budget)?;
        if matches!(referenced, LocatedStorage::ImplicitPointer { .. }) {
            return Err(EvaluateError::Unavailable(
                crate::UnsupportedVariableFeature::CrossDieEvaluation.into(),
            ));
        }
        Self::storage_with_offset(referenced, byte_offset)
    }

    /// Applies planned steps to storage, with `indices` as the values of
    /// their index step. An index outside an array's static bounds is an
    /// error; anything the program state cannot provide is an
    /// [`EvaluateError`].
    #[expect(
        clippy::too_many_arguments,
        reason = "steps run against one frame's runtime, frame base, and budget"
    )]
    #[expect(
        clippy::too_many_lines,
        reason = "each storage transition keeps its own typed failure"
    )]
    pub(super) fn apply_steps(
        &self,
        mut storage: LocatedStorage,
        steps: &[PathStep],
        indices: &[i128],
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        frame_base: &mut FrameBaseCache,
        budget: &mut InspectionBudget,
    ) -> Result<std::result::Result<LocatedStorage, EvaluateError>> {
        macro_rules! attempt {
            ($result:expr) => {
                match $result {
                    Ok(value) => value,
                    Err(error) => return Ok(Err(error)),
                }
            };
        }
        for step in steps {
            storage =
                match step {
                    PathStep::Dereference {
                        target,
                        byte_size,
                        address_class,
                    } => {
                        if let LocatedStorage::ImplicitPointer {
                            debug_info_offset,
                            byte_offset,
                        } = storage
                        {
                            attempt!(self.resolve_implicit_pointer(
                                debug_info_offset,
                                byte_offset,
                                *target,
                                address,
                                runtime,
                                frame_base,
                                budget,
                            ))
                        } else {
                            if *address_class != 0 {
                                return Ok(Err(EvaluateError::Unavailable(
                                    VariableUnavailableReason::ValueAccess(
                                        crate::ValueAccessUnavailableReason::AddressClass(
                                            *address_class,
                                        ),
                                    ),
                                )));
                            }
                            let size = attempt!(usize::try_from(*byte_size).map_err(|_| {
                                VariableUnavailableReason::EvaluationLimit.into()
                            }));
                            let (_, raw) =
                                attempt!(Self::read_storage(&storage, size, runtime, budget));
                            let pointer = attempt!(decode_address(&raw, *byte_size, self.target));
                            if pointer.get() == 0 {
                                return Ok(Err(EvaluateError::Unavailable(
                                    VariableUnavailableReason::ValueAccess(
                                        crate::ValueAccessUnavailableReason::NullPointer,
                                    ),
                                )));
                            }
                            LocatedStorage::Memory(pointer)
                        }
                    }
                    PathStep::ArrayIndex { .. } => {
                        let byte_offset = array_byte_offset(step, indices)?.unwrap_or_default();
                        attempt!(Self::storage_with_offset(storage, byte_offset))
                    }
                    PathStep::SliceIndex {
                        element_size,
                        descriptor_size,
                        has_capacity,
                    } => {
                        let [index] = indices else {
                            return Err(Error::InvalidValueExpression(format!(
                                "a slice takes one index, not {}",
                                indices.len()
                            )));
                        };
                        let index =
                            u64::try_from(*index).map_err(|_| Error::ValueIndexOutOfBounds {
                                index: *index,
                                lower_bound: 0,
                                count: 0,
                            })?;
                        let decoded = attempt!(self.decode_slice(
                            &storage,
                            *descriptor_size,
                            *has_capacity,
                            runtime,
                            budget,
                        ));
                        if index >= decoded.length {
                            return Ok(Err(EvaluateError::Unavailable(
                                VariableUnavailableReason::IndexOutOfBounds {
                                    index: i128::from(index),
                                    lower_bound: 0,
                                    count: decoded.length,
                                },
                            )));
                        }
                        let byte_offset = attempt!(
                            index
                                .checked_mul(*element_size)
                                .and_then(|offset| i64::try_from(offset).ok())
                                .ok_or_else(|| VariableUnavailableReason::EvaluationLimit.into())
                        );
                        attempt!(Self::storage_with_offset(
                            LocatedStorage::Memory(decoded.address),
                            byte_offset,
                        ))
                    }
                    PathStep::Member(step) => {
                        let PlannedMemberStep {
                            aggregate,
                            child,
                            member,
                            required_variant,
                        } = step.as_ref();
                        if let Some((required, discriminant, variants)) = required_variant {
                            let active = attempt!(self.active_variant_from_storage(
                                &storage,
                                *aggregate,
                                discriminant,
                                variants,
                                runtime,
                                budget,
                            ));
                            if active != Some(*required) {
                                let name = variants
                                    .get(*required)
                                    .and_then(|variant| variant.name.as_deref())
                                    .unwrap_or("<anonymous>");
                                return Ok(Err(EvaluateError::Unavailable(
                                    VariableUnavailableReason::ValueAccess(
                                        crate::ValueAccessUnavailableReason::InactiveVariant(Some(
                                            name.into(),
                                        )),
                                    ),
                                )));
                            }
                        }
                        match member.layout {
                            RecordMemberLayout::ByteOffset(offset) => attempt!(
                                i64::try_from(offset)
                                    .map_err(|_| VariableUnavailableReason::EvaluationLimit.into())
                                    .and_then(|offset| Self::storage_with_offset(storage, offset))
                            ),
                            RecordMemberLayout::BitRange {
                                bit_offset,
                                bit_size,
                            } => attempt!(self.bit_field_storage(
                                storage,
                                member.type_ref.id,
                                bit_offset,
                                bit_size,
                                runtime,
                                budget,
                            )),
                            RecordMemberLayout::Runtime => attempt!(self.runtime_member_storage(
                                &storage, *aggregate, *child, runtime, budget
                            )),
                        }
                    }
                    PathStep::Unavailable(reason) => {
                        return Ok(Err(EvaluateError::Unavailable(reason.clone())));
                    }
                };
        }
        Ok(Ok(storage))
    }

    pub(super) fn materialize_inspected_value(
        &self,
        type_id: TypeId,
        type_info: TypeInfo,
        storage: &LocatedStorage,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<InspectedValue> {
        let shape = match self.value_shape(type_id) {
            Ok(shape) => shape,
            Err(ValueShapeError::Malformed(description)) => {
                return Ok(inspected_value(
                    Some(type_info),
                    VariableState::Malformed(malformed_reason(
                        VariableMalformedKind::InvalidTypeGraph,
                        description,
                    )),
                    budget,
                ));
            }
            Err(ValueShapeError::Unsupported(_)) => {
                return Ok(inspected_value(
                    Some(type_info),
                    VariableState::Unavailable(
                        crate::UnsupportedVariableFeature::TypeRepresentation.into(),
                    ),
                    budget,
                ));
            }
        };
        let mut state =
            self.materialize_value_state(type_id, &shape, storage, context, runtime, budget)?;
        self.constrain_dereference(&mut state, &shape);
        Ok(inspected_value(Some(type_info), state, budget))
    }

    /// Decodes a value without its text.
    pub(super) fn decode_state(
        &self,
        type_id: TypeId,
        storage: &LocatedStorage,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<VariableState> {
        match self.value_shape(type_id) {
            Ok(shape) => {
                self.materialize_shape_state(type_id, &shape, storage, context, runtime, budget)
            }
            Err(ValueShapeError::Malformed(description)) => Ok(VariableState::Malformed(
                malformed_reason(VariableMalformedKind::InvalidTypeGraph, description),
            )),
            Err(ValueShapeError::Unsupported(_)) => Ok(VariableState::Unavailable(
                crate::UnsupportedVariableFeature::TypeRepresentation.into(),
            )),
        }
    }

    /// Materializes a value, with its text when it is a string.
    pub(super) fn materialize_value_state(
        &self,
        type_id: TypeId,
        shape: &ValueShape,
        storage: &LocatedStorage,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<VariableState> {
        let mut state =
            self.materialize_shape_state(type_id, shape, storage, context, runtime, budget)?;
        if let VariableState::Available { value, text, .. } = &mut state {
            *text = self
                .text_summary(type_id, shape, value, storage, runtime, budget)
                .map(Arc::new);
        }
        Ok(state)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "each value shape has a distinct lazy summary and child capability"
    )]
    fn materialize_shape_state(
        &self,
        type_id: TypeId,
        shape: &ValueShape,
        storage: &LocatedStorage,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<VariableState> {
        let leaf = |source, raw, value, dereference| VariableState::Available {
            source,
            raw: Some(raw),
            value,
            dereference,
            children: ValueChildren::NotApplicable,
            text: None,
        };
        let read =
            |size: u64,
             runtime: &mut dyn VariableRuntime,
             budget: &mut InspectionBudget|
             -> std::result::Result<(VariableValueSource, Arc<[u8]>), EvaluateError> {
                let size = usize::try_from(size)
                    .map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
                Self::read_storage(storage, size, runtime, budget)
            };
        Ok(match &shape.kind {
            ValueShapeKind::Scalar(base) => match read(base.byte_size, runtime, budget) {
                Ok((source, raw)) => match decode_scalar(base, &raw, self.target) {
                    Ok(value) => leaf(
                        source,
                        raw,
                        VariableValue::Scalar(value),
                        DereferenceState::NotApplicable,
                    ),
                    Err(ScalarDecodeError::Unavailable(reason)) => {
                        VariableState::Unavailable(reason)
                    }
                    Err(ScalarDecodeError::Invalid(reason)) => VariableState::Invalid {
                        source,
                        raw,
                        reason,
                    },
                    Err(ScalarDecodeError::Malformed(description)) => VariableState::Malformed(
                        malformed_reason(VariableMalformedKind::InconsistentLayout, description),
                    ),
                },
                Err(EvaluateError::Unavailable(reason)) => VariableState::Unavailable(reason),
                Err(EvaluateError::Malformed(description)) => VariableState::Malformed(
                    malformed_reason(VariableMalformedKind::InvalidExpression, description),
                ),
                Err(EvaluateError::Fatal(description)) => {
                    return Err(Error::VariableRuntime(description));
                }
            },
            ValueShapeKind::Enumeration {
                representation,
                enumerators,
                byte_size,
            } => match read(*byte_size, runtime, budget) {
                Ok((source, raw)) => {
                    match decode_integer_value(representation, &raw, self.target.byte_order) {
                        Ok(value) => {
                            let matches = enumerators
                                .iter()
                                .filter(|enumerator| enumerator.value == value)
                                .cloned()
                                .collect::<Vec<_>>()
                                .into();
                            leaf(
                                source,
                                raw,
                                VariableValue::Enumeration { value, matches },
                                DereferenceState::NotApplicable,
                            )
                        }
                        Err(description) => VariableState::Malformed(malformed_reason(
                            VariableMalformedKind::InvalidTypeGraph,
                            description,
                        )),
                    }
                }
                Err(EvaluateError::Unavailable(reason)) => VariableState::Unavailable(reason),
                Err(EvaluateError::Malformed(description)) => VariableState::Malformed(
                    malformed_reason(VariableMalformedKind::InvalidExpression, description),
                ),
                Err(EvaluateError::Fatal(description)) => {
                    return Err(Error::VariableRuntime(description));
                }
            },
            ValueShapeKind::Indirection {
                target,
                byte_size,
                address_class,
            } => match storage {
                LocatedStorage::ImplicitPointer {
                    debug_info_offset,
                    byte_offset,
                } => {
                    let dereference = target.map_or_else(
                        || DereferenceState::Unavailable {
                            pointee: None,
                            reason: DereferenceUnavailableReason::UnspecifiedPointee,
                        },
                        |target_type| {
                            DereferenceState::Available(DereferenceReference {
                                stop_id: context.stop_id,
                                thread: context.thread,
                                frame: context.frame,
                                module: context.module,
                                image: context.image,
                                context_address: context.address,
                                target_type,
                                target: crate::model::DereferenceTarget::ImplicitPointer {
                                    debug_info_offset: *debug_info_offset,
                                    byte_offset: *byte_offset,
                                },
                            })
                        },
                    );
                    VariableState::Available {
                        source: VariableValueSource::ImplicitPointer,
                        raw: None,
                        value: VariableValue::ImplicitPointer,
                        dereference,
                        children: ValueChildren::NotApplicable,
                        text: None,
                    }
                }
                _ => match read(*byte_size, runtime, budget) {
                    Ok((source, raw)) => match decode_address(&raw, *byte_size, self.target) {
                        Ok(address) => {
                            let dereference = if *address_class != 0 {
                                DereferenceState::Unavailable {
                                    pointee: None,
                                    reason: DereferenceUnavailableReason::AddressClass(
                                        *address_class,
                                    ),
                                }
                            } else if address.get() == 0 {
                                DereferenceState::Unavailable {
                                    pointee: None,
                                    reason: DereferenceUnavailableReason::Null,
                                }
                            } else if let Some(target_type) = target {
                                DereferenceState::Available(DereferenceReference {
                                    stop_id: context.stop_id,
                                    thread: context.thread,
                                    frame: context.frame,
                                    module: context.module,
                                    image: context.image,
                                    context_address: context.address,
                                    target_type: *target_type,
                                    target: crate::model::DereferenceTarget::Address(address),
                                })
                            } else {
                                DereferenceState::Unavailable {
                                    pointee: None,
                                    reason: DereferenceUnavailableReason::UnspecifiedPointee,
                                }
                            };
                            leaf(
                                source,
                                raw,
                                VariableValue::Address(AddressValue { address }),
                                dereference,
                            )
                        }
                        Err(EvaluateError::Unavailable(reason)) => {
                            VariableState::Unavailable(reason)
                        }
                        Err(EvaluateError::Malformed(description)) => {
                            VariableState::Malformed(malformed_reason(
                                VariableMalformedKind::InconsistentLayout,
                                description,
                            ))
                        }
                        Err(EvaluateError::Fatal(description)) => {
                            return Err(Error::VariableRuntime(description));
                        }
                    },
                    Err(EvaluateError::Unavailable(reason)) => VariableState::Unavailable(reason),
                    Err(EvaluateError::Malformed(description)) => VariableState::Malformed(
                        malformed_reason(VariableMalformedKind::InvalidExpression, description),
                    ),
                    Err(EvaluateError::Fatal(description)) => {
                        return Err(Error::VariableRuntime(description));
                    }
                },
            },
            ValueShapeKind::Array { dimensions, .. } => {
                let Some(total) = dimensions
                    .iter()
                    .try_fold(1_u64, |total, dimension| total.checked_mul(dimension.count))
                else {
                    return Ok(VariableState::Unavailable(
                        VariableUnavailableReason::EvaluationLimit,
                    ));
                };
                VariableState::Available {
                    source: Self::storage_source(storage),
                    raw: None,
                    value: VariableValue::Array {
                        dimensions: Arc::clone(dimensions),
                    },
                    dereference: DereferenceState::NotApplicable,
                    children: ValueChildren::Available(Self::child_reference(
                        storage, context, type_id, total, None,
                    )),
                    text: None,
                }
            }
            ValueShapeKind::Slice {
                element: _,
                byte_size,
                has_capacity,
                ..
            } => {
                let decoded =
                    match self.decode_slice(storage, *byte_size, *has_capacity, runtime, budget) {
                        Ok(value) => value,
                        Err(EvaluateError::Unavailable(reason)) => {
                            return Ok(VariableState::Unavailable(reason));
                        }
                        Err(EvaluateError::Malformed(description)) => {
                            return Ok(VariableState::Malformed(malformed_reason(
                                VariableMalformedKind::InconsistentLayout,
                                description,
                            )));
                        }
                        Err(EvaluateError::Fatal(description)) => {
                            return Err(Error::VariableRuntime(description));
                        }
                    };
                let backing = LocatedStorage::Memory(decoded.address);
                VariableState::Available {
                    source: decoded.source,
                    raw: Some(decoded.raw),
                    value: VariableValue::Slice {
                        length: decoded.length,
                        capacity: decoded.capacity,
                    },
                    dereference: DereferenceState::NotApplicable,
                    children: ValueChildren::Available(Self::child_reference(
                        &backing,
                        context,
                        type_id,
                        decoded.length,
                        None,
                    )),
                    text: None,
                }
            }
            ValueShapeKind::Record { members, bases, .. } => {
                let total =
                    u64::try_from(bases.len().saturating_add(members.len())).unwrap_or(u64::MAX);
                VariableState::Available {
                    source: Self::storage_source(storage),
                    raw: None,
                    value: VariableValue::Record,
                    dereference: DereferenceState::NotApplicable,
                    children: ValueChildren::Available(Self::child_reference(
                        storage, context, type_id, total, None,
                    )),
                    text: None,
                }
            }
            ValueShapeKind::Union { members, .. } => {
                let total = u64::try_from(members.len()).unwrap_or(u64::MAX);
                VariableState::Available {
                    source: Self::storage_source(storage),
                    raw: None,
                    value: VariableValue::Union,
                    dereference: DereferenceState::NotApplicable,
                    children: ValueChildren::Available(Self::child_reference(
                        storage, context, type_id, total, None,
                    )),
                    text: None,
                }
            }
            ValueShapeKind::Variant {
                aggregate,
                common_members,
                bases,
                discriminant,
                variants,
                ..
            } => {
                let (discriminant_value, active) = match self.variant_selection_from_storage(
                    storage,
                    *aggregate,
                    discriminant,
                    variants,
                    runtime,
                    budget,
                ) {
                    Ok(active) => active,
                    Err(EvaluateError::Unavailable(reason)) => {
                        return Ok(VariableState::Unavailable(reason));
                    }
                    Err(EvaluateError::Malformed(description)) => {
                        return Ok(VariableState::Malformed(malformed_reason(
                            VariableMalformedKind::InconsistentLayout,
                            description,
                        )));
                    }
                    Err(EvaluateError::Fatal(description)) => {
                        return Err(Error::VariableRuntime(description));
                    }
                };
                let total = bases
                    .len()
                    .saturating_add(common_members.len())
                    .saturating_add(
                        active
                            .and_then(|index| variants.get(index))
                            .map_or(0, |variant| variant.members.len()),
                    );
                VariableState::Available {
                    source: Self::storage_source(storage),
                    raw: None,
                    value: VariableValue::Variant {
                        discriminant: discriminant_value,
                        active: active
                            .and_then(|index| variants.get(index))
                            .cloned()
                            .map(Arc::new),
                    },
                    dereference: DereferenceState::NotApplicable,
                    children: ValueChildren::Available(Self::child_reference(
                        storage,
                        context,
                        type_id,
                        u64::try_from(total).unwrap_or(u64::MAX),
                        active,
                    )),
                    text: None,
                }
            }
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "child layout evaluation keeps aggregate identity, storage, type, runtime, and budget explicit"
    )]
    pub(super) fn aggregate_child_storage(
        &self,
        storage: &LocatedStorage,
        aggregate: TypeId,
        child: DynamicAggregateChild,
        type_id: TypeId,
        layout: RecordMemberLayout,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<LocatedStorage, EvaluateError> {
        match layout {
            RecordMemberLayout::ByteOffset(offset) => Self::storage_with_offset(
                storage.clone(),
                i64::try_from(offset).map_err(|_| VariableUnavailableReason::EvaluationLimit)?,
            ),
            RecordMemberLayout::BitRange {
                bit_offset,
                bit_size,
            } => self.bit_field_storage(
                storage.clone(),
                type_id,
                bit_offset,
                bit_size,
                runtime,
                budget,
            ),
            RecordMemberLayout::Runtime => {
                self.runtime_member_storage(storage, aggregate, child, runtime, budget)
            }
        }
    }

    pub(super) fn materialize_child(
        &self,
        relationship: ValueChildRelationship,
        type_id: TypeId,
        storage: std::result::Result<LocatedStorage, EvaluateError>,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<ValueChild> {
        let type_info = self
            .type_info(type_id)
            .map_err(|description| Error::debug_info(DwarfError::MalformedVariable(description)))?
            .clone();
        let state = match storage {
            Err(error) => path_error_state(error)?,
            Ok(storage) => match self.value_shape(type_id) {
                Err(ValueShapeError::Malformed(description)) => VariableState::Malformed(
                    malformed_reason(VariableMalformedKind::InvalidTypeGraph, description),
                ),
                Err(ValueShapeError::Unsupported(_)) => VariableState::Unavailable(
                    crate::UnsupportedVariableFeature::TypeRepresentation.into(),
                ),
                Ok(shape) => {
                    let mut state = self.materialize_value_state(
                        type_id, &shape, &storage, context, runtime, budget,
                    )?;
                    self.constrain_dereference(&mut state, &shape);
                    state
                }
            },
        };
        Ok(ValueChild {
            relationship,
            type_info,
            state,
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "stable child ordering and each aggregate layout are intentionally handled together"
    )]
    pub(super) fn value_child_page(
        &self,
        reference: &ValueChildrenReference,
        offset: u64,
        limit: u32,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<ValueChildPage> {
        let context = VariableContext {
            stop_id: reference.stop_id,
            thread: reference.thread,
            frame: reference.frame,
            module: reference.module,
            image: reference.image,
            address: reference.context_address,
        };
        let mut storage = Self::restored_storage(&reference.storage);
        let mut storage_failure = None;
        if let LocatedStorage::ImplicitPointer {
            debug_info_offset,
            byte_offset,
        } = storage.clone()
        {
            let mut frame_base = FrameBaseCache::Empty;
            match self.resolve_implicit_pointer(
                debug_info_offset,
                byte_offset,
                reference.target_type,
                reference.context_address,
                runtime,
                &mut frame_base,
                budget,
            ) {
                Ok(resolved) => storage = resolved,
                Err(error) => storage_failure = Some(error),
            }
        }
        let shape = self.value_shape(reference.target_type).map_err(|error| {
            let description = match error {
                ValueShapeError::Malformed(description)
                | ValueShapeError::Unsupported(description) => description,
            };
            Error::debug_info(DwarfError::MalformedVariable(description))
        })?;
        let expected_total = match &shape.kind {
            ValueShapeKind::Array { dimensions, .. } => dimensions
                .iter()
                .try_fold(1_u64, |total, dimension| total.checked_mul(dimension.count)),
            ValueShapeKind::Slice { .. } => Some(reference.total),
            ValueShapeKind::Record { members, bases, .. } => {
                u64::try_from(members.len().saturating_add(bases.len())).ok()
            }
            ValueShapeKind::Union { members, .. } => u64::try_from(members.len()).ok(),
            ValueShapeKind::Variant {
                common_members,
                bases,
                variants,
                ..
            } => {
                let active_members = reference.active_variant.map_or(Some(0), |active| {
                    variants.get(active).map(|variant| variant.members.len())
                });
                active_members.and_then(|active_members| {
                    u64::try_from(
                        bases
                            .len()
                            .saturating_add(common_members.len())
                            .saturating_add(active_members),
                    )
                    .ok()
                })
            }
            _ => None,
        };
        if expected_total != Some(reference.total) {
            return Err(Error::debug_info(DwarfError::MalformedVariable(
                "value child capability does not match its aggregate metadata".into(),
            )));
        }
        let full_requested_end = reference.total.min(offset.saturating_add(u64::from(limit)));
        if offset >= full_requested_end {
            return Ok(ValueChildPage {
                stop_id: reference.stop_id,
                offset,
                total: reference.total,
                children: Arc::from([]),
                completion: budget.completion(),
                usage: budget.usage(),
            });
        }
        let requested_end =
            full_requested_end.min(offset.saturating_add(budget.remaining_value_nodes()));
        if requested_end == offset {
            let _ = budget.consume_value_nodes(1);
            return Ok(ValueChildPage {
                stop_id: reference.stop_id,
                offset,
                total: reference.total,
                children: Arc::from([]),
                completion: budget.completion(),
                usage: budget.usage(),
            });
        }
        let mut children = Vec::with_capacity(
            usize::try_from(requested_end - offset).expect("page limit fits usize"),
        );
        // Linear pages are the common high-volume path. Capture one bounded
        // contiguous interval so scalar children do not turn a 256-element
        // page into 256 ptrace reads. If the coalesced read crosses an
        // inaccessible boundary, fall back to per-child reads so accessible
        // siblings are still reported accurately.
        let linear_storage = if storage_failure.is_some() {
            None
        } else {
            match &shape.kind {
                ValueShapeKind::Array { element, .. } | ValueShapeKind::Slice { element, .. } => {
                    let element_shape = self.value_shape(*element).ok();
                    let requires_bytes = element_shape.as_ref().is_some_and(|shape| {
                        matches!(
                            shape.kind,
                            ValueShapeKind::Scalar(_)
                                | ValueShapeKind::Enumeration { .. }
                                | ValueShapeKind::Indirection { .. }
                                | ValueShapeKind::Slice { .. }
                        )
                    });
                    let stride = element_shape.as_ref().map(ValueShape::byte_size);
                    match (&storage, stride, requires_bytes) {
                        (LocatedStorage::Memory(_), Some(stride), true) => {
                            let count = requested_end - offset;
                            let span = count
                                .checked_sub(1)
                                .and_then(|count| count.checked_mul(stride))
                                .and_then(|prefix| prefix.checked_add(stride))
                                .and_then(|span| usize::try_from(span).ok());
                            let first = offset
                                .checked_mul(stride)
                                .and_then(|offset| i64::try_from(offset).ok())
                                .and_then(|offset| {
                                    Self::storage_with_offset(storage.clone(), offset).ok()
                                });
                            match (first, span) {
                                (Some(first), Some(span))
                                    if span <= MAX_EVALUATION_MEMORY_BYTES
                                        && budget.remaining_memory_reads() != 0
                                        && u64::try_from(span).is_ok_and(|span| {
                                            span <= budget.remaining_memory_bytes()
                                        }) =>
                                {
                                    Self::read_storage(&first, span, runtime, budget).ok().map(
                                        |(source, raw)| {
                                            let end = raw.len();
                                            LocatedStorage::Bytes {
                                                source,
                                                raw,
                                                start: 0,
                                                end,
                                                address: Self::concrete_storage_address(&first),
                                            }
                                        },
                                    )
                                }
                                _ => None,
                            }
                        }
                        _ => None,
                    }
                }
                _ => None,
            }
        };
        for index in offset..requested_end {
            if budget.consume_value_nodes(1).is_err() {
                break;
            }
            let (relationship, type_id, child_storage) = match &shape.kind {
                ValueShapeKind::Array {
                    element,
                    dimensions,
                    ..
                } => {
                    let element_shape = self.value_shape(*element).map_err(|error| {
                        let description = match error {
                            ValueShapeError::Malformed(description)
                            | ValueShapeError::Unsupported(description) => description,
                        };
                        Error::debug_info(DwarfError::MalformedVariable(description))
                    })?;
                    let stride = element_shape.byte_size();
                    let storage_index = if linear_storage.is_some() {
                        index - offset
                    } else {
                        index
                    };
                    let byte_offset = storage_index
                        .checked_mul(stride)
                        .and_then(|offset| i64::try_from(offset).ok())
                        .ok_or(VariableUnavailableReason::EvaluationLimit)
                        .map_err(EvaluateError::from);
                    let child_storage = byte_offset.and_then(|offset| {
                        Self::storage_with_offset(
                            linear_storage.clone().unwrap_or_else(|| storage.clone()),
                            offset,
                        )
                    });
                    let mut source_indices = vec![0_i128; dimensions.len()];
                    let mut remaining = index;
                    for (dimension_index, dimension) in dimensions.iter().enumerate().rev() {
                        if dimension.count == 0 {
                            return Err(Error::debug_info(DwarfError::MalformedVariable(
                                "a non-empty array page has a zero-sized dimension".into(),
                            )));
                        }
                        let relative = remaining % dimension.count;
                        remaining /= dimension.count;
                        source_indices[dimension_index] = dimension
                            .lower_bound
                            .checked_add(i128::from(relative))
                            .ok_or_else(|| {
                                Error::debug_info(DwarfError::MalformedVariable(
                                    "array source index overflows".into(),
                                ))
                            })?;
                    }
                    (
                        ValueChildRelationship::ArrayElement {
                            index,
                            indices: source_indices.into(),
                        },
                        *element,
                        child_storage,
                    )
                }
                ValueShapeKind::Slice { element, .. } => {
                    let element_shape = self.value_shape(*element).map_err(|error| {
                        let description = match error {
                            ValueShapeError::Malformed(description)
                            | ValueShapeError::Unsupported(description) => description,
                        };
                        Error::debug_info(DwarfError::MalformedVariable(description))
                    })?;
                    let storage_index = if linear_storage.is_some() {
                        index - offset
                    } else {
                        index
                    };
                    let byte_offset = storage_index
                        .checked_mul(element_shape.byte_size())
                        .and_then(|offset| i64::try_from(offset).ok())
                        .ok_or(VariableUnavailableReason::EvaluationLimit)
                        .map_err(EvaluateError::from);
                    (
                        ValueChildRelationship::SliceElement { index },
                        *element,
                        byte_offset.and_then(|offset| {
                            Self::storage_with_offset(
                                linear_storage.clone().unwrap_or_else(|| storage.clone()),
                                offset,
                            )
                        }),
                    )
                }
                ValueShapeKind::Record {
                    record,
                    members,
                    bases,
                    ..
                } => {
                    let index = usize::try_from(index).expect("bounded record index fits usize");
                    if let Some(base) = bases.get(index) {
                        (
                            ValueChildRelationship::Base(base.clone()),
                            base.type_ref.id,
                            self.aggregate_child_storage(
                                &storage,
                                *record,
                                DynamicAggregateChild::Base(index),
                                base.type_ref.id,
                                base.layout,
                                runtime,
                                budget,
                            ),
                        )
                    } else {
                        let member_index = index - bases.len();
                        let member = &members[member_index];
                        (
                            ValueChildRelationship::Member(member.clone()),
                            member.type_ref.id,
                            self.aggregate_child_storage(
                                &storage,
                                *record,
                                DynamicAggregateChild::Member(member_index),
                                member.type_ref.id,
                                member.layout,
                                runtime,
                                budget,
                            ),
                        )
                    }
                }
                ValueShapeKind::Union { union, members, .. } => {
                    let member_index =
                        usize::try_from(index).expect("bounded union index fits usize");
                    let member = &members[member_index];
                    (
                        ValueChildRelationship::Member(member.clone()),
                        member.type_ref.id,
                        self.aggregate_child_storage(
                            &storage,
                            *union,
                            DynamicAggregateChild::Member(member_index),
                            member.type_ref.id,
                            member.layout,
                            runtime,
                            budget,
                        ),
                    )
                }
                ValueShapeKind::Variant {
                    aggregate,
                    common_members,
                    bases,
                    variants,
                    ..
                } => {
                    let index = usize::try_from(index).expect("bounded variant index fits usize");
                    if let Some(base) = bases.get(index) {
                        (
                            ValueChildRelationship::Base(base.clone()),
                            base.type_ref.id,
                            self.aggregate_child_storage(
                                &storage,
                                *aggregate,
                                DynamicAggregateChild::Base(index),
                                base.type_ref.id,
                                base.layout,
                                runtime,
                                budget,
                            ),
                        )
                    } else {
                        let relative = index - bases.len();
                        if let Some(member) = common_members.get(relative) {
                            (
                                ValueChildRelationship::Member(member.clone()),
                                member.type_ref.id,
                                self.aggregate_child_storage(
                                    &storage,
                                    *aggregate,
                                    DynamicAggregateChild::Member(relative),
                                    member.type_ref.id,
                                    member.layout,
                                    runtime,
                                    budget,
                                ),
                            )
                        } else {
                            let active = reference.active_variant.ok_or_else(|| {
                                Error::debug_info(DwarfError::MalformedVariable(
                                    "variant child capability has no active arm".into(),
                                ))
                            })?;
                            let member_index = relative - common_members.len();
                            let member = &variants[active].members[member_index];
                            (
                                ValueChildRelationship::Member(member.clone()),
                                member.type_ref.id,
                                self.aggregate_child_storage(
                                    &storage,
                                    *aggregate,
                                    DynamicAggregateChild::VariantMember {
                                        variant: active,
                                        member: member_index,
                                    },
                                    member.type_ref.id,
                                    member.layout,
                                    runtime,
                                    budget,
                                ),
                            )
                        }
                    }
                }
                _ => {
                    return Err(Error::debug_info(DwarfError::MalformedVariable(
                        "a non-aggregate value produced a child capability".into(),
                    )));
                }
            };
            let child_storage = storage_failure
                .clone()
                .map_or(child_storage, std::result::Result::Err);
            children.push(self.materialize_child(
                relationship,
                type_id,
                child_storage,
                context,
                runtime,
                budget,
            )?);
            if budget.exhaustion().is_some() {
                children.pop();
                break;
            }
        }
        if requested_end < full_requested_end && budget.exhaustion().is_none() {
            let _ = budget.consume_value_nodes(1);
        }
        Ok(ValueChildPage {
            stop_id: reference.stop_id,
            offset,
            total: reference.total,
            children: children.into(),
            completion: budget.completion(),
            usage: budget.usage(),
        })
    }

    pub(super) fn inspect_data_object(
        &self,
        variable: &CatalogDataObject,
        address: Option<ImageAddress>,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        frame_base_cache: &mut FrameBaseCache,
        budget: &mut InspectionBudget,
    ) -> Result<Variable> {
        if let Some(description) = &variable.malformed {
            return Ok(malformed(variable, None, Arc::clone(description)));
        }
        let type_id = match &variable.type_info {
            TypeResolution::Resolved(id) => *id,
            TypeResolution::Malformed(description) => {
                return Ok(malformed(variable, None, Arc::clone(description)));
            }
        };
        let type_info = match self.type_info(type_id) {
            Ok(info) => info.clone(),
            Err(description) => return Ok(malformed(variable, None, description)),
        };
        let shape = match self.value_shape(type_id) {
            Ok(shape) => shape,
            Err(ValueShapeError::Malformed(description)) => {
                return Ok(malformed(variable, Some(type_info), description));
            }
            Err(ValueShapeError::Unsupported(_)) => {
                return Ok(unavailable(
                    variable,
                    Some(type_info),
                    crate::UnsupportedVariableFeature::TypeRepresentation.into(),
                ));
            }
        };
        let storage =
            match self.located_data_object(variable, address, runtime, frame_base_cache, budget) {
                Ok(storage) => storage,
                Err(EvaluateError::Unavailable(reason)) => {
                    return Ok(unavailable(variable, Some(type_info), reason));
                }
                Err(EvaluateError::Malformed(description)) => {
                    return Ok(malformed(variable, Some(type_info), description));
                }
                Err(EvaluateError::Fatal(description)) => {
                    return Err(Error::VariableRuntime(description));
                }
            };
        let mut state =
            self.materialize_value_state(type_id, &shape, &storage, context, runtime, budget)?;
        self.constrain_dereference(&mut state, &shape);
        Ok(Variable {
            kind: variable.kind,
            global: None,
            name: Arc::clone(&variable.name),
            declaration: variable.declaration.clone(),
            type_info: Some(type_info),
            state,
        })
    }

    pub(super) fn constrain_dereference(&self, state: &mut VariableState, shape: &ValueShape) {
        let ValueShapeKind::Indirection {
            target: Some(target),
            ..
        } = &shape.kind
        else {
            return;
        };
        let VariableState::Available { dereference, .. } = state else {
            return;
        };
        // The pointee type describes the dereferenced expression, so it is
        // rendered against `*expr`. Resolve it once for both the downgrade of an
        // available dereference and the backfill of an already-unavailable one.
        let pointee = self.type_info(*target).ok().cloned().map(Box::new);
        match dereference {
            DereferenceState::Available(_) => {
                let reason = match self.type_info(*target) {
                    Err(description) => Some(DereferenceUnavailableReason::Malformed(
                        malformed_reason(VariableMalformedKind::InvalidTypeGraph, description),
                    )),
                    Ok(TypeInfo {
                        kind: TypeKind::Unspecified,
                        ..
                    }) => Some(DereferenceUnavailableReason::UnspecifiedPointee),
                    Ok(_) => match self.value_shape(*target) {
                        Ok(_) => None,
                        Err(ValueShapeError::Malformed(description)) => {
                            Some(DereferenceUnavailableReason::Malformed(malformed_reason(
                                VariableMalformedKind::InvalidTypeGraph,
                                description,
                            )))
                        }
                        Err(ValueShapeError::Unsupported(description)) => Some(
                            DereferenceUnavailableReason::UnsupportedPointee(description),
                        ),
                    },
                };
                if let Some(reason) = reason {
                    *dereference = DereferenceState::Unavailable { pointee, reason };
                }
            }
            DereferenceState::Unavailable { pointee: slot, .. } => {
                // Backfill the pointee metadata for reasons produced upstream
                // (a null pointer or an unsupported address class) that knew the
                // target type but did not resolve it.
                if slot.is_none() {
                    *slot = pointee;
                }
            }
            DereferenceState::NotApplicable => {}
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "dereference preserves typed type, storage, runtime, and budget failures"
    )]
    pub(super) fn dereference_value(
        &self,
        reference: &DereferenceReference,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<DereferencedValue> {
        if let Err(exhaustion) = budget.consume_value_nodes(1) {
            return Ok(DereferencedValue {
                type_info: self
                    .type_info(reference.target_type)
                    .map_err(|reason| Error::debug_info(DwarfError::MalformedVariable(reason)))?
                    .clone(),
                state: VariableState::Unavailable(exhaustion.into()),
                completion: budget.completion(),
                usage: budget.usage(),
            });
        }
        let type_info = self
            .type_info(reference.target_type)
            .map_err(|reason| Error::debug_info(DwarfError::MalformedVariable(reason)))?
            .clone();
        let shape = match self.value_shape(reference.target_type) {
            Ok(shape) => shape,
            Err(ValueShapeError::Malformed(description)) => {
                return Ok(DereferencedValue {
                    type_info,
                    state: VariableState::Malformed(malformed_reason(
                        VariableMalformedKind::InvalidTypeGraph,
                        description,
                    )),
                    completion: budget.completion(),
                    usage: budget.usage(),
                });
            }
            Err(ValueShapeError::Unsupported(_)) => {
                return Ok(DereferencedValue {
                    type_info,
                    state: VariableState::Unavailable(
                        crate::UnsupportedVariableFeature::TypeRepresentation.into(),
                    ),
                    completion: budget.completion(),
                    usage: budget.usage(),
                });
            }
        };
        let context = VariableContext {
            stop_id: reference.stop_id,
            thread: reference.thread,
            frame: reference.frame,
            module: reference.module,
            image: reference.image,
            address: reference.context_address,
        };
        let storage = match reference.target {
            crate::model::DereferenceTarget::Address(address) => LocatedStorage::Memory(address),
            crate::model::DereferenceTarget::ImplicitPointer {
                debug_info_offset,
                byte_offset,
            } => {
                let mut frame_base = FrameBaseCache::Empty;
                match self.resolve_implicit_pointer(
                    debug_info_offset,
                    byte_offset,
                    reference.target_type,
                    reference.context_address,
                    runtime,
                    &mut frame_base,
                    budget,
                ) {
                    Ok(storage) => storage,
                    Err(EvaluateError::Unavailable(reason)) => {
                        return Ok(DereferencedValue {
                            type_info,
                            state: VariableState::Unavailable(reason),
                            completion: budget.completion(),
                            usage: budget.usage(),
                        });
                    }
                    Err(EvaluateError::Malformed(description)) => {
                        return Ok(DereferencedValue {
                            type_info,
                            state: VariableState::Malformed(malformed_reason(
                                VariableMalformedKind::InvalidExpression,
                                description,
                            )),
                            completion: budget.completion(),
                            usage: budget.usage(),
                        });
                    }
                    Err(EvaluateError::Fatal(description)) => {
                        return Err(Error::VariableRuntime(description));
                    }
                }
            }
        };
        let mut state = self.materialize_value_state(
            reference.target_type,
            &shape,
            &storage,
            context,
            runtime,
            budget,
        )?;
        self.constrain_dereference(&mut state, &shape);
        Ok(DereferencedValue {
            type_info,
            state,
            completion: budget.completion(),
            usage: budget.usage(),
        })
    }
}

pub(super) fn implicit_pointer_range(
    byte_offset: i64,
    size: u64,
    containing_size: u64,
) -> std::result::Result<(u64, u64), VariableUnavailableReason> {
    let unavailable = || {
        VariableUnavailableReason::ValueAccess(
            crate::ValueAccessUnavailableReason::ImplicitPointerOutOfBounds {
                offset: byte_offset,
                size,
                containing_size,
            },
        )
    };
    let start = u64::try_from(byte_offset).map_err(|_| unavailable())?;
    let end = start
        .checked_add(size)
        .ok_or(VariableUnavailableReason::EvaluationLimit)?;
    if end > containing_size {
        return Err(unavailable());
    }
    Ok((start, end))
}

pub(super) const fn inspected_value(
    type_info: Option<TypeInfo>,
    state: VariableState,
    budget: &InspectionBudget,
) -> InspectedValue {
    InspectedValue {
        type_info,
        state,
        completion: budget.completion(),
        usage: budget.usage(),
    }
}

pub(super) fn unavailable(
    variable: &CatalogDataObject,
    type_info: Option<TypeInfo>,
    reason: VariableUnavailableReason,
) -> Variable {
    Variable {
        kind: variable.kind,
        global: None,
        name: Arc::clone(&variable.name),
        declaration: variable.declaration.clone(),
        type_info,
        state: VariableState::Unavailable(reason),
    }
}

pub(super) fn malformed(
    variable: &CatalogDataObject,
    type_info: Option<TypeInfo>,
    description: Arc<str>,
) -> Variable {
    Variable {
        kind: variable.kind,
        global: None,
        name: Arc::clone(&variable.name),
        declaration: variable.declaration.clone(),
        type_info,
        state: VariableState::Malformed(malformed_reason(
            VariableMalformedKind::InvalidAttribute,
            description,
        )),
    }
}
