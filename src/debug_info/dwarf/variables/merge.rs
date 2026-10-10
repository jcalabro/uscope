//! Walking every unit at once, and putting what each built in order.
//!
//! Each unit's walk reads what the arena held before any unit was walked
//! and builds its own types, locations, and files beside them, numbered
//! from where that base ends. Its records are then added in unit order,
//! renumbered after the units before it, which numbers everything as one
//! walk of every unit in order does, so long as no unit reaches another's
//! types: a walk in order would have built those once, where the first
//! unit reached them. A unit that does, or a load that runs out of its
//! budget or fails, is walked again in order instead.

use std::sync::Mutex;

use foldhash::HashMap;

use super::identity::{IdentityParts, PendingArguments};
use super::layered::{Layered, Marks, Rows};
use super::location::{LocationListId, LocationsBuilder};
use super::types::{
    AggregateMemberDeclaration, Bits, DieBuffers, DynamicAggregateLayoutKey, TypeArenaBuilder,
    TypeEntry,
};
use super::{Metadata, ValueDescription, WalkContext, Walked, dedup, die, row, walk_unit};
use crate::debug_info::dwarf::budget::Meter;
use crate::debug_info::dwarf::{DieMap, DwarfError, is_type_unit};
use crate::image::lines::Files;
use crate::image::locations::ExpressionId;
use crate::image::variables::TypeResolution;
use crate::{SourceFileId, SourceLocation, TypeArgument, TypeId, TypeReference};

/// Where the lists and expressions one unit pooled apart are pooled.
pub(super) struct Pooled {
    expressions: Vec<ExpressionId>,
    lists: Vec<LocationListId>,
}

impl Pooled {
    pub(super) fn expression(&self, id: ExpressionId) -> ExpressionId {
        self.expressions[id.0 as usize]
    }

    pub(super) fn list(&self, id: LocationListId) -> LocationListId {
        self.lists[id.0 as usize]
    }

    pub(super) fn frame_base(&self, frame_base: &mut Metadata<LocationListId>) {
        if let Metadata::Value(list) = frame_base {
            *list = self.list(*list);
        }
    }

    fn value(&self, value: &mut Metadata<ValueDescription>) {
        if let Metadata::Value(ValueDescription::Location(list)) = value {
            *list = self.list(*list);
        }
    }
}

/// Where the types one unit built apart are numbered.
struct Renumbered {
    /// Where the shared base ends, and the unit's own types begin.
    base: usize,
    ids: Vec<TypeId>,
}

impl Renumbered {
    fn id(&self, id: TypeId) -> TypeId {
        id.index()
            .checked_sub(self.base)
            .map_or(id, |own| self.ids[own])
    }

    fn reference(&self, reference: TypeReference) -> TypeReference {
        TypeReference {
            image: reference.image,
            id: self.id(reference.id),
        }
    }

    fn resolution(&self, resolution: &mut TypeResolution) {
        if let TypeResolution::Resolved(id) = resolution {
            *id = self.id(*id);
        }
    }
}

/// What one unit's walk built apart from the others'.
struct UnitWalk {
    result: Result<(), DwarfError>,
    walked: Walked,
    types: UnitTypes,
    files: Files,
    pool: LocationsBuilder,
}

/// The types one unit's walk built, past the shared base.
pub(super) struct UnitTypes {
    entries: Vec<TypeEntry>,
    by_die: DieMap<TypeId>,
    explicit_names: Bits,
    /// Where its identity parts begin, and the parts.
    identity_parts: (usize, Vec<Option<IdentityParts>>),
    go_identity_parts: HashMap<TypeId, super::identity::GoParts>,
    go_dict_indices: HashMap<TypeId, u64>,
    passed_by_value: HashMap<TypeId, bool>,
    pending_arguments: Vec<PendingArguments>,
    record_member_declarations: Vec<AggregateMemberDeclaration>,
    dynamic_record_layouts: HashMap<DynamicAggregateLayoutKey, ExpressionId>,
    void_type: Option<TypeId>,
    limit_type: Option<TypeId>,
    budget: Meter,
    foreign: bool,
}

/// The arena as it was before any unit was walked, which each unit's walk
/// builds on.
struct Frozen<'a, 'data> {
    dwarf: &'a gimli::Dwarf<crate::debug_info::dwarf::Reader<'data>>,
    units: &'a crate::debug_info::dwarf::Units<'data>,
    type_signatures: &'a crate::debug_info::dwarf::TypeSignatures,
    image: crate::ModuleImageId,
    context: std::sync::Arc<super::types::TypeContext>,
    by_die: Layered<DieMap<TypeId>>,
    entries: Rows<TypeEntry>,
    explicit_names: Marks,
    identity_parts: Rows<Option<IdentityParts>>,
    go_identity_parts: Layered<HashMap<TypeId, super::identity::GoParts>>,
    go_dict_indices: Layered<HashMap<TypeId, u64>>,
    passed_by_value: Layered<HashMap<TypeId, bool>>,
    byte_order: crate::ByteOrder,
    void_type: Option<TypeId>,
    budget: Meter,
}

impl<'a, 'data> TypeArenaBuilder<'a, 'data> {
    /// What every unit's walk builds on: this arena, frozen.
    fn frozen(&self) -> Frozen<'a, 'data> {
        Frozen {
            dwarf: self.dwarf,
            units: self.units,
            type_signatures: self.type_signatures,
            image: self.image,
            context: std::sync::Arc::clone(&self.context),
            by_die: self.by_die.split(),
            entries: self.entries.split(),
            explicit_names: self.explicit_names.split(),
            identity_parts: self.identity_parts.split(),
            go_identity_parts: self.go_identity_parts.split(),
            go_dict_indices: self.go_dict_indices.split(),
            passed_by_value: self.passed_by_value.split(),
            byte_order: self.byte_order,
            void_type: self.void_type,
            budget: self.budget.remaining(),
        }
    }
}

impl<'data> Frozen<'_, 'data> {
    /// An arena that builds `unit`'s types on this base, pooling locations
    /// into `pool`.
    fn split<'b>(
        &'b self,
        unit: usize,
        pool: &'b Mutex<LocationsBuilder>,
        die_buffers: &'b DieBuffers<'data>,
    ) -> TypeArenaBuilder<'b, 'data> {
        TypeArenaBuilder {
            dwarf: self.dwarf,
            units: self.units,
            type_signatures: self.type_signatures,
            image: self.image,
            by_die: self.by_die.split(),
            context: std::sync::Arc::clone(&self.context),
            entries: self.entries.split(),
            explicit_names: self.explicit_names.split(),
            pending_arguments: Vec::new(),
            resolution_depth: 0,
            byte_order: self.byte_order,
            limit_type: None,
            void_type: self.void_type,
            dynamic_record_layouts: HashMap::default(),
            pool,
            die_buffers,
            record_member_declarations: Vec::new(),
            budget: self.budget.remaining(),
            identity_parts: self.identity_parts.split(),
            go_identity_parts: self.go_identity_parts.split(),
            complex_parts: HashMap::default(),
            go_dict_indices: self.go_dict_indices.split(),
            passed_by_value: self.passed_by_value.split(),
            home_unit: Some(unit),
            foreign: false,
        }
    }
}

impl TypeArenaBuilder<'_, '_> {
    /// What this arena, split from another, built past their base.
    fn into_unit_types(self) -> UnitTypes {
        debug_assert!(
            self.complex_parts.is_empty(),
            "complex parts are added once every unit is walked"
        );
        let identity_start = self.identity_parts.base_len();
        UnitTypes {
            entries: self.entries.into_own(),
            by_die: self.by_die.into_own(),
            explicit_names: self.explicit_names.into_own(),
            identity_parts: (identity_start, self.identity_parts.into_own()),
            go_identity_parts: self.go_identity_parts.into_own(),
            go_dict_indices: self.go_dict_indices.into_own(),
            passed_by_value: self.passed_by_value.into_own(),
            pending_arguments: self.pending_arguments,
            record_member_declarations: self.record_member_declarations,
            dynamic_record_layouts: self.dynamic_record_layouts,
            void_type: self.void_type,
            limit_type: self.limit_type,
            budget: self.budget,
            foreign: self.foreign,
        }
    }

    /// Adds the types a unit built apart after these, as building them
    /// here after the units before it would have numbered them.
    #[expect(
        clippy::iter_over_hash_type,
        reason = "each entry of a map is added on its own, so their order cannot matter"
    )]
    fn absorb(&mut self, unit: UnitTypes, pooled: &Pooled) -> Renumbered {
        let base = self.entries.base_len();
        debug_assert!(
            unit.limit_type.is_none_or(|id| id.index() < base),
            "a unit that exceeds the budget is walked in order"
        );
        // The unit built `void` itself only when its base had none; a unit
        // before it may have since.
        let own_void = unit.void_type.filter(|id| id.index() >= base);
        let shared_void = own_void.and(self.void_type);
        let mut next = self.entries.len();
        let ids = (0..unit.entries.len())
            .map(|own| {
                if let Some(void) = shared_void
                    && own_void.is_some_and(|id| id.index() == base + own)
                {
                    return void;
                }
                let id = TypeId::new(u32::try_from(next).expect("bounded type count fits u32"));
                next += 1;
                id
            })
            .collect::<Vec<_>>();
        let renumbered = Renumbered { base, ids };
        for (own, entry) in unit.entries.into_iter().enumerate() {
            if shared_void.is_some() && own_void.is_some_and(|id| id.index() == base + own) {
                continue;
            }
            self.entries.push(match entry {
                TypeEntry::Resolved(mut info) => {
                    info.reference = renumbered.reference(info.reference);
                    dedup::map_references(&mut info, &mut |reference| {
                        renumbered.reference(reference)
                    });
                    TypeEntry::Resolved(info)
                }
                other => other,
            });
        }
        if self.void_type.is_none() {
            self.void_type = own_void.map(|id| renumbered.id(id));
        }
        for own in unit.explicit_names.iter() {
            let id = TypeId::new(u32::try_from(own).expect("bounded type count fits u32"));
            self.explicit_names.insert(renumbered.id(id).index());
        }
        let (start, parts) = unit.identity_parts;
        for (own, parts) in parts.into_iter().enumerate() {
            let Some(mut parts) = parts else {
                continue;
            };
            for argument in &mut parts.template {
                if let TypeArgument::Type(reference) = argument {
                    *reference = renumbered.reference(*reference);
                }
            }
            let id = TypeId::new(u32::try_from(start + own).expect("bounded type count fits u32"));
            let index = renumbered.id(id).index();
            self.identity_parts.resize_with(index + 1, || None);
            self.identity_parts[index] = Some(parts);
        }
        for (id, mut parts) in unit.go_identity_parts {
            parts.map_references(|reference| renumbered.reference(reference));
            self.go_identity_parts.insert(renumbered.id(id), parts);
        }
        for (id, index) in unit.go_dict_indices {
            self.go_dict_indices.insert(renumbered.id(id), index);
        }
        for (id, by_value) in unit.passed_by_value {
            self.passed_by_value.insert(renumbered.id(id), by_value);
        }
        self.pending_arguments
            .extend(unit.pending_arguments.into_iter().map(|mut pending| {
                let id = TypeId::new(u32::try_from(pending.entry).expect("type count fits u32"));
                pending.entry = renumbered.id(id).index();
                pending
            }));
        self.record_member_declarations
            .extend(
                unit.record_member_declarations
                    .into_iter()
                    .map(|mut declaration| {
                        declaration.aggregate = renumbered.id(declaration.aggregate);
                        declaration
                    }),
            );
        for (mut key, expression) in unit.dynamic_record_layouts {
            key.aggregate = renumbered.id(key.aggregate);
            self.dynamic_record_layouts
                .insert(key, pooled.expression(expression));
        }
        let mut by_die = unit.by_die;
        by_die.map_values(|id| renumbered.id(id));
        self.by_die.extend_own(by_die);
        let absorbed = self.budget.absorb(&unit.budget);
        debug_assert!(absorbed, "units are absorbed within the budget");
        renumbered
    }
}

/// Adds what a later unit's walk recorded, renumbered as `types`,
/// `pooled`, and `files` say.
fn absorb_walked(
    into: &mut Walked,
    unit: &mut Walked,
    types: &Renumbered,
    pooled: &Pooled,
    files: &[SourceFileId],
) {
    let objects = into.objects.len();
    let functions = into.functions.len();
    let order = into.order;
    let file = |location: &mut SourceLocation| location.file = files[location.file.index()];
    into.objects
        .extend(unit.objects.drain(..).map(|mut object| {
            types.resolution(&mut object.type_info);
            for id in [&mut object.escaped, &mut object.coroutine]
                .into_iter()
                .flatten()
            {
                *id = types.id(*id);
            }
            if let Some(declaration) = &mut object.declaration {
                file(declaration);
            }
            if let Some(declaration) = &mut object.go_declaration {
                file(&mut declaration.location);
            }
            pooled.value(&mut object.value);
            pooled.frame_base(&mut object.frame_base);
            object.order = order
                .checked_add(object.order)
                .expect("data-object DIE order overflow");
            object
        }));
    into.functions
        .extend(unit.functions.drain(..).map(|mut function| {
            for object in &mut function.objects {
                *object += row(objects);
            }
            if let Ok(captures) = &mut function.captures {
                for capture in captures {
                    types.resolution(&mut capture.type_info);
                }
            }
            if let Some(super::returns::ReturnConvention::SystemV(convention)) =
                &mut function.returns
            {
                types.resolution(&mut convention.ty);
            }
            function
        }));
    into.calls.absorb(std::mem::take(&mut unit.calls), pooled);
    into.procedures
        .extend(unit.procedures.drain(..).map(|(offset, mut location)| {
            pooled.frame_base(&mut location);
            (offset, location)
        }));
    into.vtables.extend(
        unit.vtables
            .drain(..)
            .map(|(address, id)| (address, types.id(id))),
    );
    into.go_function_entries.extend(
        unit.go_function_entries
            .drain(..)
            .map(|(address, function)| (address, function + row(functions))),
    );
    into.unnamed_parameters.extend(
        unit.unnamed_parameters
            .drain(..)
            .map(|(instance, id, object)| (instance, types.id(id), object + objects)),
    );
    into.abstract_bodies.extend(
        unit.abstract_bodies
            .drain(..)
            .map(|(instance, id)| (instance, types.id(id))),
    );
    for (instance, generics) in std::mem::take(&mut unit.function_generics) {
        let generics = generics
            .iter()
            .map(|(name, id)| (std::sync::Arc::clone(name), types.id(*id)))
            .collect();
        into.function_generics.insert(instance, generics);
    }
    into.order = order
        .checked_add(unit.order)
        .expect("data-object DIE order overflow");
}

/// What loading had recorded before any unit was walked, to go back to
/// when the units must be walked in order after all.
struct Checkpoint {
    objects: usize,
    functions: usize,
    order: u64,
    pending_arguments: usize,
    record_member_declarations: usize,
    dynamic_record_layouts: HashMap<DynamicAggregateLayoutKey, ExpressionId>,
    void_type: Option<TypeId>,
    budget: Meter,
    files: Files,
    pool: LocationsBuilder,
}

impl Checkpoint {
    fn take(
        walked: &Walked,
        types: &TypeArenaBuilder<'_, '_>,
        files: &Files,
        pool: &LocationsBuilder,
    ) -> Self {
        Self {
            objects: walked.objects.len(),
            functions: walked.functions.len(),
            order: walked.order,
            pending_arguments: types.pending_arguments.len(),
            record_member_declarations: types.record_member_declarations.len(),
            dynamic_record_layouts: types.dynamic_record_layouts.clone(),
            void_type: types.void_type,
            budget: types.budget.clone(),
            files: files.clone(),
            pool: pool.clone(),
        }
    }

    fn restore(
        self,
        walked: &mut Walked,
        types: &mut TypeArenaBuilder<'_, '_>,
        files: &mut Files,
        pool: &mut LocationsBuilder,
    ) {
        walked.objects.truncate(self.objects);
        walked.functions.truncate(self.functions);
        walked.calls = super::call_sites::CallSiteBuilder::default();
        walked.procedures.clear();
        walked.vtables.clear();
        walked.go_function_entries.clear();
        walked.unnamed_parameters.clear();
        walked.abstract_bodies.clear();
        walked.function_generics.clear();
        walked.order = self.order;
        types.by_die.clear_own();
        types.entries.clear_own();
        types.explicit_names.clear_own();
        types.identity_parts.clear_own();
        types.go_identity_parts.clear_own();
        types.go_dict_indices.clear_own();
        types.passed_by_value.clear_own();
        types.pending_arguments.truncate(self.pending_arguments);
        types
            .record_member_declarations
            .truncate(self.record_member_declarations);
        types.dynamic_record_layouts = self.dynamic_record_layouts;
        types.void_type = self.void_type;
        types.budget = self.budget;
        *files = self.files;
        *pool = self.pool;
    }
}

/// Where units' walks are added, in unit order, as they finish.
struct Merge<'m, 'a, 'data> {
    walked: &'m mut Walked,
    types: &'m mut TypeArenaBuilder<'a, 'data>,
    files: &'m mut Files,
    pool: &'m mut LocationsBuilder,
    /// The next unit to add.
    next: usize,
    /// Whether every unit added so far kept to its own types, the budget,
    /// and the tables' numbering.
    kept: bool,
}

impl Merge<'_, '_, '_> {
    fn add(&mut self, unit: Option<UnitWalk>) {
        self.next += 1;
        let Some(mut unit) = unit else {
            self.kept = false;
            return;
        };
        if !self.kept
            || unit.result.is_err()
            || unit.types.foreign
            || unit.types.budget.check().is_err()
            || unit.types.budget.spent() > self.types.budget.left()
            || u32::try_from(
                self.pool
                    .largest_table()
                    .saturating_add(unit.pool.largest_table()),
            )
            .is_err()
        {
            self.kept = false;
            return;
        }
        let (expressions, lists) = self
            .pool
            .absorb(&unit.pool)
            .expect("the tables' rows together fit their numbering");
        let pooled = Pooled { expressions, lists };
        let files = unit
            .files
            .paths()
            .iter()
            .map(|path| self.files.intern(path.clone()))
            .collect::<Vec<_>>();
        let renumbered = self.types.absorb(unit.types, &pooled);
        absorb_walked(self.walked, &mut unit.walked, &renumbered, &pooled, &files);
    }
}

/// Units finished but not yet added, by unit.
struct Finished {
    units: std::collections::BTreeMap<usize, Option<UnitWalk>>,
}

/// Walks every unit at once and adds what each recorded to `walked`,
/// `types`, `files`, and `pool` in unit order as soon as the units before
/// it are added, as walking them in order would have. Returns `false`,
/// having changed nothing, when walking them apart could differ from
/// walking them in order.
pub(super) fn walk_units_apart<'data>(
    cx: &WalkContext<'_, 'data>,
    walked: &mut Walked,
    types: &mut TypeArenaBuilder<'_, 'data>,
    files: &mut Files,
    pool: &Mutex<LocationsBuilder>,
) -> bool {
    let units = &cx.catalog.units;
    // One worker would only add copying each unit's records, and a type
    // unit's declarations name files by suffixes, which name the files
    // interned before them.
    if rayon::current_num_threads() < 2
        || units.iter().any(is_type_unit)
        || types.limit_type.is_some()
    {
        return false;
    }
    let _phase = crate::span!("variables.walk_apart");
    let frozen = types.frozen();
    let pool = &mut *pool.lock().expect("loading does not panic");
    debug_assert!(walked.calls.is_empty(), "only the walk records calls");
    let checkpoint = Checkpoint::take(walked, types, files, pool);
    let merge = Mutex::new(Merge {
        walked,
        types,
        files,
        pool,
        next: 0,
        kept: true,
    });
    let finished = Mutex::new(Finished {
        units: std::collections::BTreeMap::new(),
    });
    let abandoned = std::sync::atomic::AtomicBool::new(false);
    rayon::in_place_scope_fifo(|scope| {
        for unit_index in 0..units.len() {
            let (frozen, merge, finished, abandoned) = (&frozen, &merge, &finished, &abandoned);
            scope.spawn_fifo(move |_| {
                let unit = (!abandoned.load(std::sync::atomic::Ordering::Relaxed))
                    .then(|| walk_apart(cx, frozen, unit_index));
                finished
                    .lock()
                    .expect("loading does not panic")
                    .units
                    .insert(unit_index, unit);
                // Whichever walk finishes adds what is ready, in order. One
                // that finds another adding leaves its unit to it, which
                // looks again once it is done.
                while let Ok(mut merge) = merge.try_lock() {
                    loop {
                        let next = merge.next;
                        let Some(unit) = finished
                            .lock()
                            .expect("loading does not panic")
                            .units
                            .remove(&next)
                        else {
                            break;
                        };
                        merge.add(unit);
                        if !merge.kept {
                            abandoned.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    let next = merge.next;
                    drop(merge);
                    if !finished
                        .lock()
                        .expect("loading does not panic")
                        .units
                        .contains_key(&next)
                    {
                        break;
                    }
                }
            });
        }
    });
    let merge = merge.into_inner().expect("loading does not panic");
    debug_assert!(!merge.kept || merge.next == units.len());
    if merge.kept {
        return true;
    }
    checkpoint.restore(merge.walked, merge.types, merge.files, merge.pool);
    false
}

/// Walks one unit on the frozen arena, apart from the others.
fn walk_apart<'data>(
    cx: &WalkContext<'_, 'data>,
    frozen: &Frozen<'_, 'data>,
    unit_index: usize,
) -> UnitWalk {
    let pool = Mutex::new(LocationsBuilder::default());
    let die_buffers = DieBuffers::default();
    let mut types = frozen.split(unit_index, &pool, &die_buffers);
    let mut walked = Walked::default();
    let mut files = Files::default();
    let result = walk_unit(
        cx,
        unit_index,
        &mut walked,
        &mut types,
        &mut files,
        &mut die::DeclaredFiles::default(),
    );
    UnitWalk {
        result,
        walked,
        types: types.into_unit_types(),
        files,
        pool: pool.into_inner().expect("loading does not panic"),
    }
}
