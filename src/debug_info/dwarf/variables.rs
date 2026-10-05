//! DWARF variable inspection.
//!
//! Loading builds an immutable catalog of data objects, their scopes, and a
//! normalized type graph; inspection then evaluates locations against a
//! stopped thread through [`VariableRuntime`](crate::debug_info::VariableRuntime).
//!
//! - [`types`]: normalizing type DIEs; [`variant`]: discriminated variants.
//! - [`globals`]: the global catalog; [`die`] and [`location`]: reading DIE
//!   attributes and location descriptions.
//! - [`shape`]: how a type's bytes decode; [`codec`]: the byte-level decoding.
//! - [`evaluate`]: DWARF expression evaluation; [`inspect`]: value paths and
//!   materializing values, children, and dereferences.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use gimli::RunTimeEndian;

use crate::debug_info::{
    Accessed, Located, ObjectKey, ObjectStorage, PlannedStep, Step, VariableContext, VariableInfo,
    VariableRuntime,
};
use crate::inspection::InspectionBudget;
use crate::{
    AddressRange, ByteOrder, CodeInstanceId, DereferenceReference, DereferencedValue, Error,
    GlobalVariableId, GlobalVariableInfo, ImageAddress, InspectedValue, ModuleImageId, Result,
    SourceFile, SourceFileId, SourceLocation, TargetDescription, TypeId, TypeInfo, TypeNode,
    TypeReference, ValueChildPage, ValueChildrenReference, Variable, VariableKind,
    VariableMalformedKind, VariableMalformedReason, VariableQuery, VariableState,
};

use super::{DieKey, DwarfError, Reader, UnitCatalog, die_code_ranges, is_type_unit};
use die::{
    check_data_object_capacity, copy_name_with_origins, data_object_scope_ranges,
    declaration_with_origins, is_type_scope, origin_chain, variable_order_key,
};
use evaluate::FrameBaseCache;
use globals::{load_globals, public_global_type};
pub(in crate::debug_info) use inspect::{PathStep, array_byte_offset};
use inspect::{inspected_value, path_error_state, unavailable};
use location::{
    EvaluationUnit, Expression, LocationDescription, copy_data_object_value,
    copy_optional_location, load_evaluation_units,
};
use types::{
    DynamicAggregateLayoutKey, TypeArenaBuilder, TypeEntry, TypeMetadataEntry, TypeResolution,
};

mod codec;
mod die;
mod evaluate;
mod globals;
mod inspect;
mod location;
mod shape;
mod text;
mod types;
mod variant;

const MAX_SCALAR_BYTES: u64 = 16;
const MAX_EVALUATION_ITERATIONS: u32 = 10_000;
const MAX_EVALUATION_MEMORY_BYTES: usize = 1_024;
const MAX_LOCATION_PIECES: usize = 64;
const MAX_TYPES: usize = 65_536;
const MAX_TYPE_RESOLUTION_DEPTH: usize = 256;
const MAX_RECORD_CHILDREN: usize = 4_096;
const MAX_VARIANT_METADATA: usize = 4_096;
const MAX_SYMBOLIC_NAMES: usize = 262_144;
const MAX_AGGREGATE_DEPTH: usize = 64;
const MAX_DATA_OBJECTS: usize = 262_144;

const fn malformed_reason(
    kind: VariableMalformedKind,
    description: Arc<str>,
) -> VariableMalformedReason {
    VariableMalformedReason { kind, description }
}

#[derive(Clone)]
enum Metadata<T> {
    Value(T),
    Absent(MetadataAbsence),
    Malformed(Arc<str>),
}

#[derive(Clone, Copy)]
enum MetadataAbsence {
    NoLocation,
    NoFrameBase,
    NotApplicable,
}

#[derive(Clone)]
enum ConstantValue {
    Unsigned(u128),
    Signed(i128),
    /// A fixed-width form with implicit zero high bits.
    Fixed(u128),
    Bytes(Arc<[u8]>),
}

#[derive(Clone)]
enum ValueDescription {
    Location(LocationDescription),
    Constant(ConstantValue),
}

#[derive(Clone)]
struct CatalogDataObject {
    debug_info_offset: Option<u64>,
    kind: VariableKind,
    name: Arc<str>,
    declaration: Option<SourceLocation>,
    ranges: Arc<[AddressRange<ImageAddress>]>,
    /// The inline instance owning this variable, or `None` for the physical
    /// frame. Lookup only sees variables of the selected logical frame.
    instance: Option<CodeInstanceId>,
    lexical_depth: u32,
    order: u64,
    type_info: TypeResolution,
    value: Metadata<ValueDescription>,
    frame_base: Metadata<LocationDescription>,
    malformed: Option<Arc<str>>,
}

#[derive(Clone)]
struct Scope {
    ranges: Arc<[AddressRange<ImageAddress>]>,
    lexical_depth: u32,
    frame_base: Metadata<LocationDescription>,
    /// True within subprograms and inlined subroutines, whose formal
    /// parameters some producers, such as Zig, nest in lexical blocks.
    routine: bool,
    function: usize,
    /// The innermost containing inline instance, or `None` when the scope
    /// belongs directly to the physical frame.
    instance: Option<CodeInstanceId>,
    malformed: Option<Arc<str>>,
}

struct CatalogFunction {
    ranges: Arc<[AddressRange<ImageAddress>]>,
    objects: Vec<usize>,
}

pub(super) struct DwarfVariableInfo {
    objects: Arc<[CatalogDataObject]>,
    functions: Arc<[CatalogFunction]>,
    address_index: BTreeMap<ImageAddress, Arc<[usize]>>,
    globals: Arc<[usize]>,
    evaluation_units: Arc<[EvaluationUnit]>,
    types: Arc<[TypeNode]>,
    dynamic_record_layouts: HashMap<DynamicAggregateLayoutKey, Expression>,
    objects_by_debug_offset: HashMap<u64, usize>,
    target: TargetDescription,
    endian: RunTimeEndian,
}

pub(super) struct LoadedVariables {
    pub info: Arc<dyn VariableInfo>,
    pub globals: Vec<GlobalVariableInfo>,
    pub types: Arc<[crate::TypeNode]>,
}

#[expect(
    clippy::too_many_lines,
    reason = "one depth-first DIE walk must keep scope, variable, and parameter state synchronized"
)]
pub(super) fn load_variable_info<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    catalog: &UnitCatalog<'data>,
    target: TargetDescription,
    image_id: ModuleImageId,
    instance_ids: &HashMap<DieKey, CodeInstanceId>,
    source_files: &mut Vec<SourceFile>,
    source_file_ids: &mut HashMap<PathBuf, SourceFileId>,
) -> std::result::Result<LoadedVariables, DwarfError> {
    let units = catalog.units.as_slice();
    let mut objects = Vec::new();
    let mut functions = Vec::new();
    let mut order = 0_u64;
    let evaluation_units = load_evaluation_units(units)?;
    let mut types = TypeArenaBuilder::new(
        dwarf,
        units,
        &catalog.type_signatures,
        image_id,
        target.byte_order,
    );
    let (mut globals, global_objects) = load_globals(
        dwarf,
        units,
        &mut objects,
        &mut order,
        source_files,
        source_file_ids,
        &mut types,
    )?;

    for (unit_index, unit) in units.iter().enumerate() {
        if is_type_unit(unit) {
            continue;
        }
        let mut entries = unit.entries();
        let mut scopes = Vec::<Option<Scope>>::new();

        while let Some(entry) = entries.next_dfs()? {
            let depth =
                usize::try_from(entry.depth()).map_err(|_| DwarfError::InvalidEntryDepth)?;
            scopes.truncate(depth);
            let parent = scopes.last().and_then(Clone::clone);

            let scope = match entry.tag() {
                gimli::DW_TAG_subprogram => {
                    let ranges =
                        die_code_ranges(dwarf, unit, entry, &catalog.code).map(Arc::<[_]>::from)?;
                    let function = functions.len();
                    functions.push(CatalogFunction {
                        ranges: Arc::clone(&ranges),
                        objects: Vec::new(),
                    });
                    Some(Scope {
                        ranges,
                        lexical_depth: 0,
                        frame_base: copy_optional_location(
                            dwarf,
                            unit_index,
                            unit,
                            entry.attr_value(gimli::DW_AT_frame_base),
                            MetadataAbsence::NoFrameBase,
                        ),
                        routine: true,
                        function,
                        instance: None,
                        malformed: None,
                    })
                }
                gimli::DW_TAG_lexical_block => parent.as_ref().map(|parent| {
                    let (ranges, malformed) =
                        match die_code_ranges(dwarf, unit, entry, &catalog.code)
                            .map(Arc::<[_]>::from)
                        {
                            Ok(ranges) if !ranges.is_empty() => (ranges, None),
                            Ok(_) => (Arc::clone(&parent.ranges), None),
                            Err(error) => {
                                (Arc::clone(&parent.ranges), Some(error.to_string().into()))
                            }
                        };
                    Scope {
                        ranges,
                        lexical_depth: parent.lexical_depth.saturating_add(1),
                        frame_base: parent.frame_base.clone(),
                        routine: parent.routine,
                        function: parent.function,
                        instance: parent.instance,
                        malformed: malformed.or_else(|| parent.malformed.clone()),
                    }
                }),
                // An inline instance keeps the caller's frame base and function
                // while narrowing to its own code ranges. Unlike a lexical
                // block, an instance with no usable ranges must not widen to
                // the caller's extent: give it an empty extent so its locals
                // and parameters can never contaminate lookups.
                gimli::DW_TAG_inlined_subroutine => parent.as_ref().map(|parent| {
                    let instance = instance_ids
                        .get(&DieKey {
                            unit: unit_index,
                            offset: entry.offset().0,
                        })
                        .copied();
                    let (ranges, malformed) =
                        match die_code_ranges(dwarf, unit, entry, &catalog.code)
                            .map(Arc::<[_]>::from)
                        {
                            Ok(ranges) if ranges.is_empty() => (
                                Vec::new().into(),
                                Some(Arc::from("inlined subroutine has no address ranges")),
                            ),
                            // A ranged instance must be identified so lookups can
                            // scope to it; without an identity its contents could
                            // only be misattributed.
                            Ok(_) if instance.is_none() => (
                                Vec::new().into(),
                                Some(Arc::from("inlined subroutine has no code instance")),
                            ),
                            Ok(ranges) => (ranges, None),
                            Err(error) => (Vec::new().into(), Some(error.to_string().into())),
                        };
                    Scope {
                        ranges,
                        lexical_depth: parent.lexical_depth.saturating_add(1),
                        frame_base: parent.frame_base.clone(),
                        routine: true,
                        function: parent.function,
                        instance,
                        malformed: malformed.or_else(|| parent.malformed.clone()),
                    }
                }),
                tag if is_type_scope(tag) => None,
                _ => parent.clone(),
            };
            // An empty extent is deliberate containment (a rangeless inline
            // instance) and must stay empty through every descendant scope;
            // only a nested subprogram starts an independent extent.
            let scope = if entry.tag() != gimli::DW_TAG_subprogram
                && parent
                    .as_ref()
                    .is_some_and(|parent| parent.ranges.is_empty())
            {
                scope.map(|mut scope| {
                    scope.ranges = Vec::new().into();
                    scope
                })
            } else {
                scope
            };

            let kind = match entry.tag() {
                gimli::DW_TAG_variable => Some(VariableKind::Local),
                gimli::DW_TAG_formal_parameter => Some(VariableKind::Parameter),
                _ => None,
            };
            if let Some(kind) = kind {
                let owning_scope = parent.as_ref().filter(|scope| {
                    !scope.ranges.is_empty() && (kind == VariableKind::Local || scope.routine)
                });
                if let Some(scope) = owning_scope {
                    // Concrete inline-instance entries reference their
                    // abstract origin for descriptive metadata.
                    let (chain, chain_error) = match origin_chain(units, unit_index, entry) {
                        Ok(chain) => (chain, None),
                        Err(error) => (Vec::new(), Some(Arc::from(error.to_string()))),
                    };
                    let object_name = match kind {
                        VariableKind::Parameter => "parameter",
                        VariableKind::Local => "variable",
                        VariableKind::Global => "global",
                    };
                    let (name, name_error) =
                        match copy_name_with_origins(dwarf, units, unit, entry, &chain) {
                            Ok(Some(name)) => (name, None),
                            Ok(None) => (
                                format!("<anonymous {object_name} at {:#x}>", entry.offset().0)
                                    .into(),
                                Some(Arc::from(format!("{object_name} has no name"))),
                            ),
                            Err(error) => (
                                format!("<malformed {object_name} at {:#x}>", entry.offset().0)
                                    .into(),
                                Some(error.to_string().into()),
                            ),
                        };
                    order = order
                        .checked_add(1)
                        .expect("data-object DIE order overflow");
                    let declaration = declaration_with_origins(
                        dwarf,
                        units,
                        unit,
                        entry,
                        &chain,
                        source_files,
                        source_file_ids,
                    );
                    let (ranges, scope_error) = data_object_scope_ranges(scope, entry);
                    let (type_unit, type_value) = entry
                        .attr_value(gimli::DW_AT_type)
                        .map(|value| (unit_index, Some(value)))
                        .or_else(|| {
                            chain.iter().find_map(|(origin_unit, origin_entry)| {
                                origin_entry
                                    .attr_value(gimli::DW_AT_type)
                                    .map(|value| (*origin_unit, Some(value)))
                            })
                        })
                        .unwrap_or((unit_index, None));
                    check_data_object_capacity(objects.len())?;
                    functions[scope.function].objects.push(objects.len());
                    objects.push(CatalogDataObject {
                        debug_info_offset: entry
                            .offset()
                            .to_debug_info_offset(&unit.header)
                            .map(|offset| u64::try_from(offset.0).expect("DWARF offset fits u64")),
                        kind,
                        name,
                        declaration: declaration.as_ref().ok().cloned().flatten(),
                        ranges,
                        instance: scope.instance,
                        lexical_depth: scope.lexical_depth,
                        order,
                        type_info: types.variable_type(type_unit, type_value),
                        value: copy_data_object_value(dwarf, unit_index, unit, entry),
                        frame_base: scope.frame_base.clone(),
                        malformed: declaration
                            .err()
                            .map(|error| error.to_string().into())
                            .or(scope_error)
                            .or_else(|| scope.malformed.clone())
                            .or(chain_error)
                            .or(name_error),
                    });
                }
            }

            scopes.push(scope);
        }
    }

    for function in &mut functions {
        function
            .objects
            .sort_by_key(|index| variable_order_key(&objects[*index]));
    }
    let mut address_index = BTreeMap::<ImageAddress, Vec<usize>>::new();
    for (function, metadata) in functions.iter().enumerate() {
        for range in metadata.ranges.iter() {
            address_index.entry(range.start).or_default().push(function);
        }
    }
    let objects_by_debug_offset = objects
        .iter()
        .enumerate()
        .filter_map(|(index, object)| object.debug_info_offset.map(|offset| (offset, index)))
        .collect();
    types.populate_go_named_constants();
    types.populate_record_member_declarations(source_files, source_file_ids);
    types.finalize_type_graph();
    assert_eq!(
        globals.len(),
        global_objects.len(),
        "every global catalog entry has one evaluation object"
    );
    for (global, object) in globals.iter_mut().zip(&global_objects) {
        let object = objects
            .get(*object)
            .expect("global catalog references a known evaluation object");
        global.type_info = public_global_type(&object.type_info, &types.entries);
    }
    let finalized_types = std::mem::take(&mut types.entries)
        .into_iter()
        .enumerate()
        .map(|(index, entry)| match entry {
            TypeEntry::Resolved(info) => TypeNode::Resolved(info),
            TypeEntry::Malformed(description) => TypeNode::Malformed {
                reference: TypeReference {
                    image: image_id,
                    id: TypeId::new(u32::try_from(index).expect("type count fits u32")),
                },
                description,
            },
            TypeEntry::Building => TypeNode::Malformed {
                reference: TypeReference {
                    image: image_id,
                    id: TypeId::new(u32::try_from(index).expect("type count fits u32")),
                },
                description: "type graph did not finish building".into(),
            },
        })
        .collect::<Arc<[_]>>();
    Ok(LoadedVariables {
        info: Arc::new(DwarfVariableInfo {
            objects: objects.into(),
            functions: functions.into(),
            address_index: address_index
                .into_iter()
                .map(|(address, functions)| (address, functions.into()))
                .collect(),
            globals: global_objects.into(),
            evaluation_units: evaluation_units.into(),
            types: Arc::clone(&finalized_types),
            dynamic_record_layouts: types.dynamic_record_layouts,
            objects_by_debug_offset,
            target,
            endian: match target.byte_order {
                ByteOrder::Little => RunTimeEndian::Little,
                ByteOrder::Big => RunTimeEndian::Big,
            },
        }),
        globals,
        types: finalized_types,
    })
}

impl VariableInfo for DwarfVariableInfo {
    fn inspect(
        &self,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        query: &VariableQuery,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<Variable>> {
        let Some(function) = self.function_at(address) else {
            return match query {
                VariableQuery::All => Ok(Vec::new()),
                VariableQuery::Name(name) => Err(Error::VariableNotFound(name.clone())),
                VariableQuery::Global(global) => {
                    Err(Error::VariableNotFound(global.variable.to_string()))
                }
            };
        };
        let active = || {
            function
                .objects
                .iter()
                .map(|&index| &self.objects[index])
                .filter(|object| object.instance == selected)
                .filter(|object| object.ranges.iter().any(|range| range.contains(address)))
        };
        if matches!(query, VariableQuery::All) {
            let mut frame_base = FrameBaseCache::Empty;
            let mut variables = Vec::new();
            for object in active() {
                if budget.consume_variable_value().is_err() {
                    break;
                }
                variables.push(self.inspect_data_object(
                    object,
                    Some(address),
                    context,
                    runtime,
                    &mut frame_base,
                    budget,
                )?);
            }
            return Ok(variables);
        }
        let selected_objects = match query {
            VariableQuery::Name(name) => {
                let mut named = active()
                    .filter(|object| object.name.as_ref() == name)
                    .collect::<Vec<_>>();
                let Some(depth) = named.iter().map(|object| object.lexical_depth).max() else {
                    return Err(Error::VariableNotFound(name.clone()));
                };
                named.retain(|object| object.lexical_depth == depth);
                if named.len() != 1 {
                    return Err(Error::AmbiguousVariable(name.clone()));
                }
                named
            }
            VariableQuery::Global(global) => {
                return Err(Error::VariableNotFound(global.variable.to_string()));
            }
            VariableQuery::All => unreachable!("all-variable lookup returned above"),
        };
        let mut frame_base = FrameBaseCache::Empty;
        let mut variables = Vec::new();
        for object in selected_objects {
            if budget.consume_variable_value().is_err() {
                break;
            }
            variables.push(self.inspect_data_object(
                object,
                Some(address),
                context,
                runtime,
                &mut frame_base,
                budget,
            )?);
        }
        Ok(variables)
    }

    fn visible_object(
        &self,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        name: &str,
    ) -> Result<ObjectKey> {
        self.visible_object(address, selected, name).map(ObjectKey)
    }

    fn global_object(&self, id: GlobalVariableId) -> Result<ObjectKey> {
        self.globals
            .get(id.index())
            .map(|&index| ObjectKey(index))
            .ok_or_else(|| Error::VariableNotFound(id.to_string()))
    }

    fn object_type(&self, object: ObjectKey) -> std::result::Result<TypeId, Arc<str>> {
        match &self.objects[object.0].type_info {
            TypeResolution::Resolved(id) => Ok(*id),
            TypeResolution::Malformed(description) => Err(Arc::clone(description)),
        }
    }

    fn type_info(&self, id: TypeId) -> std::result::Result<TypeInfo, Arc<str>> {
        Self::type_info(self, id).cloned()
    }

    fn plan_step(&self, from: TypeId, step: Step<'_>) -> Result<PlannedStep> {
        Self::plan_step(self, from, step)
    }

    fn inspect_object(
        &self,
        object: ObjectKey,
        address: Option<ImageAddress>,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Variable> {
        let mut frame_base = FrameBaseCache::Empty;
        self.inspect_data_object(
            &self.objects[object.0],
            address,
            context,
            runtime,
            &mut frame_base,
            budget,
        )
    }

    fn locate(
        &self,
        object: ObjectKey,
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Accessed> {
        let variable = &self.objects[object.0];
        let mut frame_base = FrameBaseCache::Empty;
        let ty = match &variable.type_info {
            TypeResolution::Resolved(id) => *id,
            TypeResolution::Malformed(description) => {
                return Ok(Err(VariableState::Malformed(malformed_reason(
                    VariableMalformedKind::InvalidTypeGraph,
                    Arc::clone(description),
                ))));
            }
        };
        match self.located_data_object(variable, address, runtime, &mut frame_base, budget) {
            Ok(storage) => Ok(Ok(Located {
                ty,
                storage: Self::retained_storage(&storage),
            })),
            Err(error) => path_error_state(error).map(Err),
        }
    }

    fn apply(
        &self,
        from: &Located,
        step: &PlannedStep,
        indices: &[i128],
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Accessed> {
        let mut frame_base = FrameBaseCache::Empty;
        let storage = Self::restored_storage(&from.storage);
        match self.apply_steps(
            storage,
            &step.steps,
            indices,
            address,
            runtime,
            &mut frame_base,
            budget,
        )? {
            Ok(storage) => {
                let Some(ty) = step.result else {
                    return Ok(Err(VariableState::Malformed(malformed_reason(
                        VariableMalformedKind::InvalidExpression,
                        "an untyped step unexpectedly reached storage".into(),
                    ))));
                };
                Ok(Ok(Located {
                    ty,
                    storage: Self::retained_storage(&storage),
                }))
            }
            Err(error) => path_error_state(error).map(Err),
        }
    }

    fn materialize(
        &self,
        at: &Located,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<InspectedValue> {
        let type_info = match Self::type_info(self, at.ty) {
            Ok(info) => info.clone(),
            Err(description) => {
                return Ok(inspected_value(
                    None,
                    VariableState::Malformed(malformed_reason(
                        VariableMalformedKind::InvalidTypeGraph,
                        description,
                    )),
                    budget,
                ));
            }
        };
        self.materialize_inspected_value(
            at.ty,
            type_info,
            &Self::restored_storage(&at.storage),
            context,
            runtime,
            budget,
        )
    }

    fn local_storage(
        &self,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        root: &str,
    ) -> Result<ObjectStorage> {
        Ok(self.object_storage(&self.objects[self.visible_object(address, selected, root)?]))
    }

    fn global_storage(&self, id: GlobalVariableId) -> Result<ObjectStorage> {
        let global_index = id.index();
        let object_index = *self
            .globals
            .get(global_index)
            .ok_or_else(|| Error::VariableNotFound(id.to_string()))?;
        Ok(self.object_storage(&self.objects[object_index]))
    }

    fn inspect_global(
        &self,
        id: GlobalVariableId,
        address: Option<ImageAddress>,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Variable> {
        let global_index = id.index();
        let object_index = *self
            .globals
            .get(global_index)
            .ok_or_else(|| Error::VariableNotFound(id.to_string()))?;
        let object = &self.objects[object_index];
        if let Err(exhaustion) = budget.consume_variable_value() {
            return Ok(unavailable(object, None, exhaustion.into()));
        }
        let mut frame_base = FrameBaseCache::Empty;
        self.inspect_data_object(object, address, context, runtime, &mut frame_base, budget)
    }

    fn dereference(
        &self,
        reference: &DereferenceReference,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<DereferencedValue> {
        self.dereference_value(reference, runtime, budget)
    }

    fn value_children(
        &self,
        reference: &ValueChildrenReference,
        offset: u64,
        limit: u32,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<ValueChildPage> {
        self.value_child_page(reference, offset, limit, runtime, budget)
    }
}

impl TypeMetadataEntry for TypeNode {
    fn type_info(&self) -> std::result::Result<&TypeInfo, Arc<str>> {
        match self {
            Self::Resolved(info) => Ok(info),
            Self::Malformed { description, .. } => Err(Arc::clone(description)),
        }
    }
}

#[cfg(feature = "fuzzing")]
pub(super) fn fuzz_expression(data: &[u8]) {
    use evaluate::{FrameBase, evaluate};

    use crate::debug_info::{VariableRuntime, VariableRuntimeError};
    use crate::{ImageAddress, VariableUnavailableReason, VirtualAddress};

    struct FuzzRuntime;

    impl VariableRuntime for FuzzRuntime {
        fn register(
            &mut self,
            register: u16,
        ) -> std::result::Result<crate::debug_info::VariableRegister, VariableRuntimeError>
        {
            Err(VariableRuntimeError::Unavailable(
                VariableUnavailableReason::RegisterUnavailable(register.to_string().into()),
            ))
        }

        fn call_frame_cfa(&self) -> std::result::Result<VirtualAddress, VariableRuntimeError> {
            Err(VariableRuntimeError::Unavailable(
                VariableUnavailableReason::CallFrameUnavailable(
                    crate::CallFrameUnavailableReason::NoInstructionContext,
                ),
            ))
        }

        fn tls_address(
            &mut self,
            _offset: u64,
        ) -> std::result::Result<VirtualAddress, VariableUnavailableReason> {
            Err(VariableUnavailableReason::TlsUnavailable(
                crate::TlsUnavailableReason::ProviderUnavailable,
            ))
        }

        fn relocate(&self, address: ImageAddress) -> std::result::Result<VirtualAddress, Arc<str>> {
            Ok(VirtualAddress::new(address.get()))
        }

        fn read_memory(
            &mut self,
            address: VirtualAddress,
            size: usize,
        ) -> std::result::Result<Arc<[u8]>, VariableRuntimeError> {
            Err(VariableRuntimeError::Unavailable(
                VariableUnavailableReason::MemoryInaccessible {
                    address,
                    requested: u64::try_from(size).unwrap_or(u64::MAX),
                    completed: 0,
                    next_address: address,
                },
            ))
        }
    }

    let expression = Expression {
        bytes: Arc::from(&data[..data.len().min(MAX_EVALUATION_MEMORY_BYTES)]),
        encoding: gimli::Encoding {
            format: gimli::Format::Dwarf32,
            version: 5,
            address_size: 8,
        },
        unit: 0,
        indexed_addresses: Arc::new(HashMap::new()),
    };
    let _ = evaluate(
        &expression,
        RunTimeEndian::Little,
        &mut FrameBase::Unsupported,
        &[],
        &mut FuzzRuntime,
        &mut InspectionBudget::default(),
    );
}

#[cfg(test)]
mod tests;
