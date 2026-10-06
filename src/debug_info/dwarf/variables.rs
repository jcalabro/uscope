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
    check_data_object_capacity, copy_name, data_object_scope_ranges, debug_info_offset,
    declaration_with_origins, is_type_scope, origin_chain, strict_flag, string_with_origins,
    type_with_origins, variable_order_key,
};
use evaluate::FrameBaseCache;
use globals::{load_globals, public_global_type};
pub(in crate::debug_info) use inspect::{PathStep, array_byte_offset};
use inspect::{data_object, evaluate_error_state, inspected_value};
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
mod identity;
mod inspect;
mod location;
mod pieces;
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
    /// For a variable Go moved to the heap, which its debug information
    /// names `&name`, the type of the pointer its location holds; the
    /// variable is what that points to.
    escaped: Option<TypeId>,
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
    /// Whether the scope is in a subprogram's definition, abstract or
    /// concrete, rather than in a declaration inside a type.
    defined: bool,
}

struct CatalogFunction {
    ranges: Arc<[AddressRange<ImageAddress>]>,
    objects: Vec<usize>,
    /// The name a Go function value calling it shows.
    name: Option<Arc<str>>,
    /// The variables a Go closure captured, in its context, or why they
    /// cannot be known.
    captures: std::result::Result<Vec<Capture>, Arc<str>>,
}

/// One variable a Go closure captured: a copy of its value, or, when its
/// name begins with `&`, a pointer to the variable.
#[derive(Clone)]
struct Capture {
    name: Arc<str>,
    /// Its offset in the closure's context, past the code pointer.
    offset: u64,
    type_info: TypeResolution,
}

/// Go's `DW_AT_go_closure_offset`: where in a closure's context a captured
/// variable is.
const DW_AT_GO_CLOSURE_OFFSET: gimli::DwAt = gimli::DwAt(0x2907);

pub(super) struct DwarfVariableInfo {
    objects: Arc<[CatalogDataObject]>,
    functions: Arc<[CatalogFunction]>,
    address_index: BTreeMap<ImageAddress, Arc<[usize]>>,
    globals: Arc<[usize]>,
    evaluation_units: Arc<[EvaluationUnit]>,
    types: Arc<[TypeNode]>,
    dynamic_record_layouts: HashMap<DynamicAggregateLayoutKey, Expression>,
    /// Go functions by the address their code begins at, which a func
    /// value holds.
    go_function_entries: HashMap<ImageAddress, usize>,
    /// The float type of complex numbers' parts, by the part's name and size.
    complex_parts: HashMap<(Arc<str>, u64), TypeId>,
    objects_by_debug_offset: HashMap<u64, usize>,
    target: TargetDescription,
    endian: RunTimeEndian,
}

pub(super) struct LoadedVariables {
    pub info: Arc<dyn VariableInfo>,
    pub globals: Vec<GlobalVariableInfo>,
    pub types: Arc<[crate::TypeNode]>,
    /// Rust trait objects' vtables, by address, with the concrete type each
    /// is for.
    pub vtables: Vec<(ImageAddress, TypeReference)>,
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
    let mut vtables = Vec::new();
    let mut go_function_entries = HashMap::new();
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
                    let defined = strict_flag(entry, gimli::DW_AT_declaration) == Ok(false);
                    // The types a function's code uses are the program's
                    // types too, though no data holds them: a view may name
                    // the type only an inlined function returns.
                    if defined {
                        types.reach(unit_index, entry.attr_value(gimli::DW_AT_type));
                    }
                    let ranges =
                        die_code_ranges(dwarf, unit, entry, &catalog.code).map(Arc::<[_]>::from)?;
                    let function = functions.len();
                    let go = evaluation_units[unit_index].language == Some(gimli::DW_LANG_Go);
                    if go && defined {
                        // A func value holds the address its code begins at.
                        let entry_address = entry
                            .attr_value(gimli::DW_AT_low_pc)
                            .map(|value| dwarf.attr_address(unit, value))
                            .transpose()?
                            .flatten()
                            .map(ImageAddress::new)
                            .filter(|address| ranges.iter().any(|range| range.contains(*address)));
                        if let Some(address) = entry_address {
                            go_function_entries.insert(address, function);
                        }
                    }
                    functions.push(CatalogFunction {
                        ranges: Arc::clone(&ranges),
                        objects: Vec::new(),
                        // An optimized closure's out-of-line code names
                        // its abstract origin.
                        name: if go {
                            origin_chain(units, unit_index, entry)
                                .ok()
                                .and_then(|chain| {
                                    string_with_origins(
                                        dwarf,
                                        units,
                                        unit,
                                        entry,
                                        &chain,
                                        gimli::DW_AT_name,
                                    )
                                    .ok()
                                })
                                .flatten()
                        } else {
                            None
                        },
                        captures: Ok(Vec::new()),
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
                        defined,
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
                        defined: parent.defined,
                    }
                }),
                // An inline instance keeps the caller's frame base and function
                // but only its own code ranges; one without usable ranges gets
                // none, unlike a lexical block, so its locals never match.
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
                        defined: parent.defined,
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

            if depth == 1
                && entry.tag() == gimli::DW_TAG_variable
                && let Some((address, ty)) = rust_vtable(dwarf, unit_index, unit, entry, &mut types)
            {
                vtables.push((address, ty));
            }
            // A Go interface may hold a value of any type the runtime
            // describes, which no data need mention.
            if depth == 1
                && types::is_type_die_tag(entry.tag())
                && identity::go_runtime_type(entry).is_some()
            {
                types.resolve(DieKey {
                    unit: unit_index,
                    offset: entry.offset().0,
                });
            }
            let kind = match entry.tag() {
                gimli::DW_TAG_variable => Some(VariableKind::Local),
                gimli::DW_TAG_formal_parameter => Some(VariableKind::Parameter),
                _ => None,
            };
            if entry.tag() == gimli::DW_TAG_variable
                && let Some(scope) = parent.as_ref().filter(|scope| scope.routine)
                && let Some(offset) = entry.attr_value(DW_AT_GO_CLOSURE_OFFSET)
            {
                let (type_unit, type_value) = type_with_origins(unit_index, entry, &[]);
                let capture = match (offset.udata_value(), copy_name(dwarf, unit, entry)) {
                    (Some(offset), Ok(Some(name))) => Ok(Capture {
                        name,
                        offset,
                        type_info: types.variable_type(type_unit, type_value),
                    }),
                    _ => Err(Arc::from("a closure's captured variable is malformed")),
                };
                // One capture it cannot describe leaves them all unknown,
                // rather than the closure seeming to capture less.
                let captures = &mut functions[scope.function].captures;
                match capture {
                    Ok(capture) => {
                        if let Ok(captures) = captures {
                            captures.push(capture);
                        }
                    }
                    Err(reason) => *captures = Err(reason),
                }
            }
            if let Some(kind) = kind {
                let owning_scope = parent.as_ref().filter(|scope| {
                    !scope.ranges.is_empty() && (kind == VariableKind::Local || scope.routine)
                });
                // A variable of code with no address of its own, such as an
                // abstract inline instance, names no value, but its type is
                // the program's.
                if owning_scope.is_none() && parent.as_ref().is_some_and(|scope| scope.defined) {
                    types.reach(unit_index, entry.attr_value(gimli::DW_AT_type));
                }
                if let Some(scope) = owning_scope {
                    // Concrete inline-instance entries reference their
                    // abstract origin for descriptive metadata.
                    let (chain, chain_error) = match origin_chain(units, unit_index, entry) {
                        Ok(chain) => (chain, None),
                        Err(error) => (Vec::new(), Some(Arc::from(error.to_string()))),
                    };
                    // Go marks its results as variable parameters.
                    let kind = if kind == VariableKind::Parameter
                        && evaluation_units[unit_index].language == Some(gimli::DW_LANG_Go)
                        && entry
                            .attr_value(gimli::DW_AT_variable_parameter)
                            .or_else(|| {
                                chain.iter().find_map(|(_, origin)| {
                                    origin.attr_value(gimli::DW_AT_variable_parameter)
                                })
                            })
                            .is_some_and(|value| {
                                matches!(value, gimli::AttributeValue::Flag(true))
                                    || value.udata_value() == Some(1)
                            }) {
                        VariableKind::Result
                    } else {
                        kind
                    };
                    let object_name = match kind {
                        VariableKind::Parameter => "parameter",
                        VariableKind::Result => "result",
                        VariableKind::Local => "variable",
                        VariableKind::Global => "global",
                    };
                    let (name, name_error) = match string_with_origins(
                        dwarf,
                        units,
                        unit,
                        entry,
                        &chain,
                        gimli::DW_AT_name,
                    ) {
                        Ok(Some(name)) => (name, None),
                        Ok(None) => (
                            format!("<anonymous {object_name} at {:#x}>", entry.offset().0).into(),
                            Some(Arc::from(format!("{object_name} has no name"))),
                        ),
                        Err(error) => (
                            format!("<malformed {object_name} at {:#x}>", entry.offset().0).into(),
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
                    let (type_unit, type_value) = type_with_origins(unit_index, entry, &chain);
                    let type_info = types.variable_type(type_unit, type_value);
                    // Go names a variable it moved to the heap `&name`, and
                    // describes the pointer to it.
                    let go = evaluation_units[unit_index].language == Some(gimli::DW_LANG_Go);
                    let (name, type_info, escaped) = match (go, name.strip_prefix('&')) {
                        (true, Some(variable)) => {
                            let variable = Arc::from(variable);
                            match type_info {
                                TypeResolution::Resolved(pointer) => match types.pointee(pointer) {
                                    Some(target) => (
                                        variable,
                                        TypeResolution::Resolved(target),
                                        Some(pointer),
                                    ),
                                    None => (
                                        variable,
                                        TypeResolution::Malformed(
                                            "a variable Go moved to the heap is not described by a pointer"
                                                .into(),
                                        ),
                                        None,
                                    ),
                                },
                                malformed @ TypeResolution::Malformed(_) => {
                                    (variable, malformed, None)
                                }
                            }
                        }
                        _ => (name, type_info, None),
                    };
                    check_data_object_capacity(objects.len())?;
                    functions[scope.function].objects.push(objects.len());
                    objects.push(CatalogDataObject {
                        debug_info_offset: debug_info_offset(unit, entry),
                        kind,
                        name,
                        declaration: declaration.as_ref().ok().cloned().flatten(),
                        ranges,
                        instance: scope.instance,
                        lexical_depth: scope.lexical_depth,
                        order,
                        type_info,
                        escaped,
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
        .map(|(index, entry)| {
            let description = match entry {
                TypeEntry::Resolved(info) => return TypeNode::Resolved(info),
                TypeEntry::Malformed(description) => description,
                TypeEntry::Building => "type graph did not finish building".into(),
            };
            TypeNode::Malformed {
                reference: TypeReference {
                    image: image_id,
                    id: TypeId::new(u32::try_from(index).expect("type count fits u32")),
                },
                description,
            }
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
            go_function_entries,
            complex_parts: types.complex_parts,
            objects_by_debug_offset,
            target,
            endian: match target.byte_order {
                ByteOrder::Little => RunTimeEndian::Little,
                ByteOrder::Big => RunTimeEndian::Big,
            },
        }),
        globals,
        types: finalized_types,
        vtables: vtables
            .into_iter()
            .map(|(address, id)| {
                (
                    ImageAddress::new(address),
                    TypeReference {
                        image: image_id,
                        id,
                    },
                )
            })
            .collect(),
    })
}

/// A Rust trait object's vtable, `<C as Trait>::{vtable}`: a variable at a
/// fixed address, whose type names `C` as its containing type.
fn rust_vtable<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'data>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
    types: &mut TypeArenaBuilder<'_, 'data>,
) -> Option<(u64, TypeId)> {
    let name = die::copy_name(dwarf, unit, entry).ok()??;
    if !name.ends_with("::{vtable}") {
        return None;
    }
    let gimli::AttributeValue::Exprloc(expression) = entry.attr_value(gimli::DW_AT_location)?
    else {
        return None;
    };
    let mut operations = expression.operations(unit.encoding());
    let address = match operations.next().ok()?? {
        gimli::Operation::Address { address } => address,
        gimli::Operation::AddressIndex { index } => dwarf.address(unit, index).ok()?,
        _ => return None,
    };
    if operations.next().ok()?.is_some() {
        return None;
    }
    let vtable_type = types.reference(unit_index, entry.attr_value(gimli::DW_AT_type))?;
    let vtable_entry = types.units[vtable_type.unit]
        .entry(gimli::UnitOffset(vtable_type.offset))
        .ok()?;
    let concrete = types.reference(
        vtable_type.unit,
        vtable_entry.attr_value(gimli::DW_AT_containing_type),
    )?;
    Some((address, types.resolve(concrete)))
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
        let objects = match query {
            VariableQuery::All => self.function_at(address).map_or_else(Vec::new, |function| {
                function
                    .objects
                    .iter()
                    .map(|&index| &self.objects[index])
                    .filter(|object| {
                        object.instance == selected
                            && object.ranges.iter().any(|range| range.contains(address))
                    })
                    .collect()
            }),
            VariableQuery::Name(name) => {
                vec![&self.objects[self.visible_object(address, selected, name)?]]
            }
            VariableQuery::Global(global) => {
                return Err(Error::VariableNotFound(global.variable.to_string()));
            }
        };
        let mut frame_base = FrameBaseCache::Empty;
        let mut variables = Vec::new();
        for object in objects {
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

    fn plan_step(&self, from: TypeId, step: Step<'_>) -> Result<PlannedStep> {
        Self::plan_step(self, from, step)
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
            Ok(storage) => Ok(Ok(Located { ty, storage })),
            Err(error) => {
                evaluate_error_state(error, VariableMalformedKind::InvalidExpression).map(Err)
            }
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
        let storage = from.storage.clone();
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
                Ok(Ok(Located { ty, storage }))
            }
            Err(error) => {
                evaluate_error_state(error, VariableMalformedKind::InvalidExpression).map(Err)
            }
        }
    }

    fn load(
        &self,
        at: &Located,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<std::result::Result<crate::VariableValue, VariableState>> {
        match self.decode_state(at.ty, &at.storage, context, runtime, budget)? {
            VariableState::Available { value, .. } => Ok(Ok(value)),
            state => Ok(Err(state)),
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
        self.materialize_inspected_value(at.ty, type_info, &at.storage, context, runtime, budget)
    }

    fn object_storage(&self, object: ObjectKey) -> ObjectStorage {
        self.object_storage(&self.objects[object.0])
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
            return Ok(data_object(
                object,
                None,
                VariableState::Unavailable(exhaustion.into()),
            ));
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

        fn image_address(&self, address: VirtualAddress) -> Option<ImageAddress> {
            Some(ImageAddress::new(address.get()))
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
