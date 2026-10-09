//! Types no query can tell apart become one. Each unit describes the types
//! it uses, so a program built from many units holds many copies of each:
//! tokio-server's 60,839 types are 8,313 distinct ones.
//!
//! Two types merge only when everything about them is equal, the types
//! they refer to included, however deep and however cyclic: their
//! unfoldings are the same tree, so every walk of one is a walk of the
//! other. The coarsest such partition is found by refinement, starting
//! from each type's own fields and splitting until the types it refers to
//! agree. A type that keeps its unit's provenance, or whose layout is
//! computed at run time, is never merged.

use std::sync::Arc;

use std::collections::HashMap;

use foldhash::HashSet;
use rayon::prelude::*;

use super::types::{BuiltTypes, TypeEntry, TypeResolution};
use crate::type_identity::ANONYMOUS_NAMESPACE;
use crate::{
    BaseClass, RecordMember, TypeArgument, TypeId, TypeIdentity, TypeInfo, TypeKind, TypeReference,
    Variant, VariantDiscriminant,
};

/// How many rounds refinement may take. Each round costs one pass over
/// every reference, and real programs settle in a few; a module that does
/// not keeps all its types.
const MAX_ROUNDS: usize = 64;

/// Where each type went.
#[derive(Debug)]
pub(super) struct Remap {
    ids: Vec<TypeId>,
    retained: usize,
}

impl Remap {
    /// The new identifier of the type `id` named. An identifier past the
    /// types stays past them.
    pub(super) fn id(&self, id: TypeId) -> TypeId {
        self.ids.get(id.index()).copied().unwrap_or_else(|| {
            let beyond = id.get() - u32::try_from(self.ids.len()).expect("type count fits u32");
            TypeId::new(u32::try_from(self.retained).expect("type count fits u32") + beyond)
        })
    }

    pub(super) fn resolution(&self, resolution: &mut TypeResolution) {
        if let TypeResolution::Resolved(id) = resolution {
            *id = self.id(*id);
        }
    }

    fn reference(&self, reference: TypeReference) -> TypeReference {
        TypeReference {
            image: reference.image,
            id: self.id(reference.id),
        }
    }
}

impl BuiltTypes {
    /// Merges the types no query can tell apart, keeping the first of each
    /// in identifier order and renumbering the rest densely. Returns where
    /// each type went, or `None` when no two types merged.
    pub(super) fn deduplicate(&mut self) -> Option<Remap> {
        self.deduplicate_with(&foldhash::fast::FixedState::default())
    }

    /// [`Self::deduplicate`], hashing with `hasher`, which only chooses
    /// candidates: equal hashes never merge types that differ.
    pub(super) fn deduplicate_with<H: std::hash::BuildHasher + Clone + Sync>(
        &mut self,
        hasher: &H,
    ) -> Option<Remap> {
        let signatures = {
            let _phase = crate::span!("types.deduplicate.signatures");
            self.signatures()
        };
        let classes = {
            let _phase = crate::span!("types.deduplicate.refine");
            refine(signatures, hasher)?
        };
        let mut new_of_class = vec![u32::MAX; classes.len()];
        let mut retained = 0_u32;
        let mut kept = vec![false; classes.len()];
        let ids = classes
            .iter()
            .zip(&mut kept)
            .map(|(class, kept)| {
                let slot = &mut new_of_class[*class as usize];
                if *slot == u32::MAX {
                    *slot = retained;
                    retained += 1;
                    *kept = true;
                }
                TypeId::new(*slot)
            })
            .collect::<Vec<_>>();
        let merged = ids.len() - retained as usize;
        crate::count!("types_merged", merged);
        if merged == 0 {
            return None;
        }
        let remap = Remap {
            ids,
            retained: retained as usize,
        };
        let _phase = crate::span!("types.deduplicate.apply");
        self.apply(&remap, &kept);
        Some(remap)
    }

    /// Each type's own fields, with every reference zeroed, and the
    /// references in the order a walk of its fields meets them.
    #[expect(
        clippy::disallowed_methods,
        reason = "which aggregates have run-time layouts is one set in any order"
    )]
    fn signatures(&self) -> Signatures {
        let count = self.entries.len();
        let layouts = self
            .dynamic_record_layouts
            .keys()
            .map(|key| key.aggregate)
            .collect::<HashSet<_>>();
        let each = self
            .entries
            .par_iter()
            .enumerate()
            .map(|(index, entry)| {
                let id = TypeId::new(u32::try_from(index).expect("type count fits u32"));
                let TypeEntry::Resolved(info) = entry else {
                    return None;
                };
                if layouts.contains(&id) || local(info) {
                    return None;
                }
                let mut edges = Vec::new();
                let mut normalized = map_references(info, &mut |reference| {
                    edges.push(reference);
                    zero(reference)
                });
                normalized.reference = zero(normalized.reference);
                // A type that refers outside the graph keeps its own
                // identifier, so it never merges.
                if edges.iter().any(|reference| {
                    reference.image != info.reference.image || reference.id.index() >= count
                }) {
                    return None;
                }
                let fields = Fields {
                    info: normalized,
                    go_dict_index: self.go_dict_indices.get(&id).copied(),
                    passed_by_value: self.passed_by_value.get(&id).copied(),
                };
                Some((fields, edges))
            })
            .collect::<Vec<_>>();
        let mut signatures = Signatures {
            fields: Vec::with_capacity(count),
            offsets: Vec::with_capacity(count + 1),
            edges: Vec::new(),
        };
        signatures.offsets.push(0);
        for signature in each {
            // A type that cannot merge refers to nothing refinement must
            // follow.
            let fields = signature.map(|(fields, edges)| {
                signatures
                    .edges
                    .extend(edges.iter().map(|reference| reference.id.get()));
                fields
            });
            signatures.fields.push(fields);
            signatures.offsets.push(signatures.edges.len());
        }
        signatures
    }

    /// Keeps the first type of each class, pointing every reference where
    /// its type went.
    #[expect(
        clippy::disallowed_methods,
        clippy::iter_over_hash_type,
        reason = "each identifier moves on its own, so order does not matter"
    )]
    fn apply(&mut self, remap: &Remap, kept: &[bool]) {
        // An indexed collect splits alike however the work is stolen.
        let entries = std::mem::take(&mut self.entries)
            .into_par_iter()
            .enumerate()
            .map(|(index, entry)| {
                kept[index].then(|| match entry {
                    TypeEntry::Resolved(info) => {
                        let mut info =
                            map_references(&info, &mut |reference| remap.reference(reference));
                        info.reference.id = remap.ids[index];
                        TypeEntry::Resolved(info)
                    }
                    other => other,
                })
            })
            .collect::<Vec<_>>();
        self.entries = Vec::with_capacity(remap.retained);
        self.entries.extend(entries.into_iter().flatten());
        self.dynamic_record_layouts = std::mem::take(&mut self.dynamic_record_layouts)
            .into_iter()
            .map(|(mut key, layout)| {
                key.aggregate = remap.id(key.aggregate);
                (key, layout)
            })
            .collect();
        for id in self.complex_parts.values_mut() {
            *id = remap.id(*id);
        }
        // Merged types agree on these, which their signatures include.
        self.go_dict_indices = std::mem::take(&mut self.go_dict_indices)
            .into_iter()
            .map(|(id, index)| (remap.id(id), index))
            .collect();
        self.passed_by_value = std::mem::take(&mut self.passed_by_value)
            .into_iter()
            .map(|(id, by_value)| (remap.id(id), by_value))
            .collect();
    }
}

/// What a type is apart from the types it refers to.
#[derive(PartialEq, Eq, Hash)]
struct Fields {
    info: TypeInfo,
    go_dict_index: Option<u64>,
    passed_by_value: Option<bool>,
}

struct Signatures {
    /// Each type's fields, or `None` for one that never merges.
    fields: Vec<Option<Fields>>,
    /// Where each type's references start in `edges`.
    offsets: Vec<usize>,
    edges: Vec<u32>,
}

/// Whether a type belongs to its unit alone, as one in an anonymous
/// namespace does.
fn local(info: &TypeInfo) -> bool {
    info.identity.as_ref().is_some_and(|identity| {
        identity
            .path
            .iter()
            .any(|segment| segment.as_ref() == ANONYMOUS_NAMESPACE)
    })
}

const fn zero(reference: TypeReference) -> TypeReference {
    TypeReference {
        image: reference.image,
        id: TypeId::new(0),
    }
}

/// The coarsest partition in which types of one class have equal fields
/// and refer, position by position, to types of one class. Returns each
/// type's class, numbered by first appearance, or `None` when refinement
/// does not settle within [`MAX_ROUNDS`].
fn refine<H: std::hash::BuildHasher + Clone + Sync>(
    signatures: Signatures,
    hasher: &H,
) -> Option<Vec<u32>> {
    let Signatures {
        fields,
        offsets,
        edges,
    } = signatures;
    let count = fields.len();
    let mut classes = classify(&fields, hasher);
    // Dropping thousands of payloads is work too.
    fields.into_par_iter().for_each(drop);
    let mut distinct = classes
        .iter()
        .copied()
        .max()
        .map_or(0, |last| last as usize + 1);
    let mut targets = vec![0_u32; edges.len()];
    for round in 1..=MAX_ROUNDS {
        for (target, edge) in targets.iter_mut().zip(&edges) {
            *target = classes[*edge as usize];
        }
        let mut seen = HashMap::with_capacity_and_hasher(distinct, hasher.clone());
        let refined = (0..count)
            .map(|index| {
                let key = (classes[index], &targets[offsets[index]..offsets[index + 1]]);
                let next = u32::try_from(seen.len()).expect("type count fits u32");
                *seen.entry(key).or_insert(next)
            })
            .collect::<Vec<_>>();
        // Refinement only splits classes, so an equal count is the same
        // partition.
        let settled = seen.len() == distinct;
        distinct = seen.len();
        classes = refined;
        if settled {
            crate::count!("types_dedup_rounds", round);
            return Some(classes);
        }
    }
    crate::count!("types_dedup_abandoned", 1);
    None
}

/// Numbers the types by their fields alone, in order of first appearance;
/// a type that never merges has a class of its own. Hashes are computed in
/// parallel and only propose the earlier type a type might equal; the
/// fields are then compared.
fn classify<H: std::hash::BuildHasher + Sync>(fields: &[Option<Fields>], hasher: &H) -> Vec<u32> {
    let digests = fields
        .par_iter()
        .map(|fields| fields.as_ref().map(|fields| hasher.hash_one(fields)))
        .collect::<Vec<_>>();
    let mut first_of_hash = foldhash::HashMap::default();
    let candidates = digests
        .iter()
        .enumerate()
        .map(|(index, hash)| hash.map_or(index, |hash| *first_of_hash.entry(hash).or_insert(index)))
        .collect::<Vec<_>>();
    let equal = candidates
        .par_iter()
        .enumerate()
        .map(|(index, candidate)| *candidate == index || fields[*candidate] == fields[index])
        .collect::<Vec<_>>();
    // A type unequal to the first of its hash equals only types that are
    // unequal to it too, so those are compared among themselves.
    let mut collided = HashMap::with_hasher(foldhash::fast::FixedState::default());
    let mut classes = Vec::<u32>::with_capacity(fields.len());
    let mut next = 0_u32;
    let mut fresh = || {
        next += 1;
        next - 1
    };
    for (index, candidate) in candidates.iter().enumerate() {
        let class = match &fields[index] {
            None => fresh(),
            Some(_) if *candidate == index => fresh(),
            Some(_) if equal[index] => classes[*candidate],
            Some(own) => *collided.entry(own).or_insert_with(&mut fresh),
        };
        classes.push(class);
    }
    classes
}

/// A copy of `info` whose every reference to a type, its own aside, is
/// `map` of the original, met in one fixed order.
#[expect(clippy::too_many_lines, reason = "one arm for each kind of type")]
pub(super) fn map_references(
    info: &TypeInfo,
    map: &mut impl FnMut(TypeReference) -> TypeReference,
) -> TypeInfo {
    let kind = match &info.kind {
        TypeKind::Enumeration {
            representation,
            underlying,
            enumerators,
            origin,
            scoped,
        } => TypeKind::Enumeration {
            representation: representation.clone(),
            underlying: underlying.map(&mut *map),
            enumerators: Arc::clone(enumerators),
            origin: *origin,
            scoped: *scoped,
        },
        TypeKind::Pointer {
            target,
            address_class,
        } => TypeKind::Pointer {
            target: target.map(&mut *map),
            address_class: *address_class,
        },
        TypeKind::Reference {
            kind,
            target,
            address_class,
        } => TypeKind::Reference {
            kind: *kind,
            target: map(*target),
            address_class: *address_class,
        },
        TypeKind::Array {
            element,
            dimensions,
        } => TypeKind::Array {
            element: map(*element),
            dimensions: Arc::clone(dimensions),
        },
        TypeKind::Slice {
            element,
            has_capacity,
            text,
        } => TypeKind::Slice {
            element: map(*element),
            has_capacity: *has_capacity,
            text: *text,
        },
        TypeKind::Record {
            kind,
            members,
            bases,
            incomplete,
        } => TypeKind::Record {
            kind: *kind,
            members: map_members(members, map),
            bases: map_bases(bases, map),
            incomplete: *incomplete,
        },
        TypeKind::Union {
            members,
            incomplete,
        } => TypeKind::Union {
            members: map_members(members, map),
            incomplete: *incomplete,
        },
        TypeKind::Variant {
            storage,
            common_members,
            bases,
            discriminant,
            variants,
            incomplete,
        } => TypeKind::Variant {
            storage: *storage,
            common_members: map_members(common_members, map),
            bases: map_bases(bases, map),
            discriminant: Box::new(match discriminant.as_ref() {
                VariantDiscriminant::Stored(member) => {
                    VariantDiscriminant::Stored(map_member(member, map))
                }
                VariantDiscriminant::TagType(target) => VariantDiscriminant::TagType(map(*target)),
                VariantDiscriminant::Absent => VariantDiscriminant::Absent,
            }),
            variants: variants
                .iter()
                .map(|variant| Variant {
                    name: variant.name.clone(),
                    selection: variant.selection.clone(),
                    members: map_members(&variant.members, map),
                })
                .collect(),
            incomplete: *incomplete,
        },
        TypeKind::Modified { modifier, target } => TypeKind::Modified {
            modifier: *modifier,
            target: map(*target),
        },
        TypeKind::Named {
            target,
            relationship,
        } => TypeKind::Named {
            target: target.map(&mut *map),
            relationship: *relationship,
        },
        TypeKind::Signature {
            returns,
            parameters,
            variadic,
            prototyped,
        } => TypeKind::Signature {
            returns: returns.map(&mut *map),
            parameters: parameters.iter().map(|parameter| map(*parameter)).collect(),
            variadic: *variadic,
            prototyped: *prototyped,
        },
        kind @ (TypeKind::Base(_)
        | TypeKind::Unspecified
        | TypeKind::Function
        | TypeKind::Opaque { .. }) => kind.clone(),
    };
    let identity = info.identity.as_ref().map(|identity| {
        if identity
            .arguments
            .iter()
            .any(|argument| matches!(argument, TypeArgument::Type(_)))
        {
            Arc::new(TypeIdentity {
                arguments: identity
                    .arguments
                    .iter()
                    .map(|argument| match argument {
                        TypeArgument::Type(reference) => TypeArgument::Type(map(*reference)),
                        other => other.clone(),
                    })
                    .collect(),
                ..TypeIdentity::clone(identity)
            })
        } else {
            Arc::clone(identity)
        }
    });
    TypeInfo {
        reference: info.reference,
        name: Arc::clone(&info.name),
        byte_size: info.byte_size,
        kind,
        identity,
    }
}

fn map_member(
    member: &RecordMember,
    map: &mut impl FnMut(TypeReference) -> TypeReference,
) -> RecordMember {
    RecordMember {
        type_ref: map(member.type_ref),
        ..member.clone()
    }
}

/// `members` with each reference mapped; an empty list stays itself.
fn map_members(
    members: &Arc<[RecordMember]>,
    map: &mut impl FnMut(TypeReference) -> TypeReference,
) -> Arc<[RecordMember]> {
    if members.is_empty() {
        return Arc::clone(members);
    }
    members
        .iter()
        .map(|member| map_member(member, map))
        .collect()
}

fn map_bases(
    bases: &Arc<[BaseClass]>,
    map: &mut impl FnMut(TypeReference) -> TypeReference,
) -> Arc<[BaseClass]> {
    if bases.is_empty() {
        return Arc::clone(bases);
    }
    bases
        .iter()
        .map(|base| BaseClass {
            type_ref: map(base.type_ref),
            ..base.clone()
        })
        .collect()
}

#[cfg(test)]
mod tests;
