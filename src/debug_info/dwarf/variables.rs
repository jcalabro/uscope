use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use gimli::{EvaluationResult, Location, Reader as _, RunTimeEndian, Value};

use super::{DieKey, DwarfError, Reader, die_reference, source_file_id, source_path};
use crate::debug_info::{VariableContext, VariableInfo, VariableRuntime};
use crate::{
    AddressRange, AddressValue, Architecture, BaseType, BaseTypeEncoding, ByteOrder,
    CodeInstanceId, ColumnNumber, DereferenceReference, DereferenceState,
    DereferenceUnavailableReason, DereferencedValue, Error, FloatValue, GlobalVariableId,
    GlobalVariableInfo, GlobalVariableType, GlobalVariableVisibility, ImageAddress, LineNumber,
    ModuleImageId, ReferenceKind, Result, ScalarValue, SourceFile, SourceFileId, SourceLocation,
    TargetDescription, TypeId, TypeInfo, TypeKind, TypeQualifier, TypeReference, Variable,
    VariableKind, VariableMalformedReason, VariableQuery, VariableState, VariableUnavailableReason,
    VariableValue, VariableValueSource, VirtualAddress,
};

const MAX_SCALAR_BYTES: u64 = 16;
const MAX_EVALUATION_ITERATIONS: u32 = 10_000;
const MAX_EVALUATION_MEMORY_READS: u32 = 64;
const MAX_EVALUATION_MEMORY_BYTES: usize = 1_024;
const MAX_LOCATION_PIECES: usize = 64;

#[derive(Default)]
struct EvaluationBudget {
    memory_reads: u32,
    memory_bytes: usize,
}

impl EvaluationBudget {
    fn consume_memory(
        &mut self,
        size: usize,
    ) -> std::result::Result<(), VariableUnavailableReason> {
        self.memory_reads = self
            .memory_reads
            .checked_add(1)
            .ok_or(VariableUnavailableReason::EvaluationLimit)?;
        self.memory_bytes = self
            .memory_bytes
            .checked_add(size)
            .ok_or(VariableUnavailableReason::EvaluationLimit)?;
        if self.memory_reads > MAX_EVALUATION_MEMORY_READS
            || self.memory_bytes > MAX_EVALUATION_MEMORY_BYTES
        {
            return Err(VariableUnavailableReason::EvaluationLimit);
        }
        Ok(())
    }
}

enum FrameBaseCache {
    Empty,
    Available(VirtualAddress),
    Unavailable(VariableUnavailableReason),
    Malformed(Arc<str>),
}

/// How an evaluation may obtain the frame base if the executed expression
/// path actually requires one.
enum FrameBase<'a> {
    Unsupported,
    Lazy(FrameBaseContext<'a>),
}

struct FrameBaseContext<'a> {
    location: &'a Metadata<LocationDescription>,
    address: Option<ImageAddress>,
    cache: &'a mut FrameBaseCache,
}

/// An evaluation failure that preserves the unavailable-versus-malformed
/// distinction of the frame-base metadata it may consult.
#[derive(Debug, PartialEq)]
enum EvaluateError {
    Unavailable(VariableUnavailableReason),
    Malformed(Arc<str>),
}

impl From<VariableUnavailableReason> for EvaluateError {
    fn from(reason: VariableUnavailableReason) -> Self {
        Self::Unavailable(reason)
    }
}

impl From<Arc<str>> for EvaluateError {
    fn from(description: Arc<str>) -> Self {
        Self::Unavailable(description.into())
    }
}

impl From<&str> for EvaluateError {
    fn from(description: &str) -> Self {
        Self::Unavailable(description.into())
    }
}

impl From<crate::UnsupportedVariableFeature> for EvaluateError {
    fn from(feature: crate::UnsupportedVariableFeature) -> Self {
        Self::Unavailable(feature.into())
    }
}

#[derive(Clone)]
struct Expression {
    bytes: Arc<[u8]>,
    encoding: gimli::Encoding,
    unit: usize,
    indexed_addresses: Arc<HashMap<usize, u64>>,
}

struct EvaluationUnit {
    base_types: HashMap<usize, gimli::ValueType>,
}

#[derive(Clone, Copy)]
struct ImplicitPointerLocation {
    debug_info_offset: u64,
    byte_offset: i64,
    size_in_bits: Option<u64>,
    bit_offset: Option<u64>,
}

#[derive(Clone)]
struct LocationEntry {
    range: Option<AddressRange<ImageAddress>>,
    expression: Expression,
}

#[derive(Clone)]
struct LocationDescription {
    entries: Arc<[LocationEntry]>,
}

impl LocationDescription {
    fn expression(
        &self,
        address: Option<ImageAddress>,
    ) -> std::result::Result<Option<&Expression>, VariableUnavailableReason> {
        // Specific ranged entries override default (range-less) entries per
        // DWARF 5 default-location semantics. Without an instruction context we
        // cannot select a ranged entry; a range-less default still resolves, but
        // an entry that only exists behind a range must fail explicitly rather
        // than silently resolve against a guessed address.
        if let Some(address) = address {
            let mut specific = self
                .entries
                .iter()
                .filter(|entry| entry.range.is_some_and(|range| range.contains(address)));
            if let Some(entry) = specific.next() {
                if specific.next().is_some() {
                    return Err("multiple locations are active at the current instruction".into());
                }
                return Ok(Some(&entry.expression));
            }
        }
        let mut defaults = self.entries.iter().filter(|entry| entry.range.is_none());
        let expression = defaults.next().map(|entry| &entry.expression);
        if defaults.next().is_some() {
            return Err("multiple default locations were supplied".into());
        }
        if address.is_none() && expression.is_none() && !self.entries.is_empty() {
            // A global was requested without a valid module-relative instruction,
            // yet every location entry is range-gated. Refuse to guess.
            return Err("no instruction context to select this object's location".into());
        }
        Ok(expression)
    }
}

#[derive(Clone)]
enum TypeResolution {
    Resolved(TypeId),
    Malformed(Arc<str>),
}

#[derive(Clone)]
enum TypeEntry {
    Building,
    Resolved(TypeInfo),
    Malformed(Arc<str>),
}

struct TypeArenaBuilder<'a, 'data> {
    dwarf: &'a gimli::Dwarf<Reader<'data>>,
    units: &'a [gimli::Unit<Reader<'data>>],
    image: ModuleImageId,
    by_die: HashMap<DieKey, TypeId>,
    entries: Vec<TypeEntry>,
}

#[derive(Clone, Debug)]
enum ValueShape {
    Scalar(BaseType),
    Indirection {
        target: Option<TypeId>,
        byte_size: u64,
        address_class: u64,
    },
}

#[derive(Clone)]
enum Metadata<T> {
    Value(T),
    Unavailable(Arc<str>),
    Malformed(Arc<str>),
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
    /// True for subprograms and inlined subroutines, whose direct children
    /// may include formal parameters.
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
    types: Arc<[TypeEntry]>,
    objects_by_debug_offset: HashMap<u64, usize>,
    target: TargetDescription,
    endian: RunTimeEndian,
}

pub(super) struct LoadedVariables {
    pub info: Arc<dyn VariableInfo>,
    pub globals: Vec<GlobalVariableInfo>,
}

#[derive(Clone, Default)]
struct GlobalScope {
    path: Arc<[Arc<str>]>,
    routine: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum DefinitionResolution {
    New,
    Existing(usize),
    Conflict,
}

#[derive(Default)]
struct DefinitionIndex {
    by_identity: HashMap<Arc<str>, usize>,
}

impl DefinitionIndex {
    fn resolve(&mut self, identities: &[Arc<str>], candidate: usize) -> DefinitionResolution {
        let mut existing = identities
            .iter()
            .filter_map(|identity| self.by_identity.get(identity).copied())
            .collect::<Vec<_>>();
        existing.sort_unstable();
        existing.dedup();
        match existing.as_slice() {
            [] => {
                for identity in identities {
                    self.by_identity.insert(Arc::clone(identity), candidate);
                }
                DefinitionResolution::New
            }
            [definition] => {
                for identity in identities {
                    self.by_identity.insert(Arc::clone(identity), *definition);
                }
                DefinitionResolution::Existing(*definition)
            }
            _ => {
                // Contradictory producer identities must remain separate and
                // therefore ambiguous; never select one convincing value.
                for identity in identities {
                    self.by_identity
                        .entry(Arc::clone(identity))
                        .or_insert(candidate);
                }
                DefinitionResolution::Conflict
            }
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "global collection deliberately resolves producer variants in one auditable pass"
)]
fn load_globals<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    units: &[gimli::Unit<Reader<'data>>],
    objects: &mut Vec<CatalogDataObject>,
    order: &mut u64,
    source_files: &mut Vec<SourceFile>,
    source_file_ids: &mut HashMap<PathBuf, SourceFileId>,
    types: &mut TypeArenaBuilder<'_, 'data>,
) -> std::result::Result<(Vec<GlobalVariableInfo>, Vec<usize>), DwarfError> {
    let mut scopes_by_die = HashMap::<DieKey, GlobalScope>::new();

    // Pass one records lexical ownership for every DIE. A later definition
    // may point backward to a declaration nested in a namespace or class.
    for (unit_index, unit) in units.iter().enumerate() {
        let mut entries = unit.entries();
        let mut scopes = Vec::<GlobalScope>::new();
        while let Some(entry) = entries.next_dfs()? {
            let depth =
                usize::try_from(entry.depth()).map_err(|_| DwarfError::InvalidEntryDepth)?;
            scopes.truncate(depth);
            let parent = scopes.last().cloned().unwrap_or_default();
            let mut scope = parent.clone();
            match entry.tag() {
                gimli::DW_TAG_subprogram | gimli::DW_TAG_inlined_subroutine => {
                    scope.routine = true;
                }
                gimli::DW_TAG_namespace
                | gimli::DW_TAG_module
                | gimli::DW_TAG_class_type
                | gimli::DW_TAG_structure_type
                | gimli::DW_TAG_union_type => {
                    let component = match copy_name(dwarf, unit, entry) {
                        Ok(Some(name)) => name,
                        Ok(None) if entry.tag() == gimli::DW_TAG_namespace => {
                            Arc::from("{anonymous}")
                        }
                        Ok(None) => Arc::from("{anonymous type}"),
                        Err(error) => Arc::from(format!("{{malformed scope: {error}}}")),
                    };
                    let mut path = parent.path.to_vec();
                    path.push(component);
                    scope.path = path.into();
                }
                _ => {}
            }
            scopes_by_die.insert(
                DieKey {
                    unit: unit_index,
                    offset: entry.offset().0,
                },
                scope.clone(),
            );
            scopes.push(scope);
        }
    }

    let mut globals = Vec::<GlobalVariableInfo>::new();
    let mut global_objects = Vec::<usize>::new();
    let mut definitions = DefinitionIndex::default();

    // Pass two resolves every non-routine data object independently.
    for (unit_index, unit) in units.iter().enumerate() {
        let mut entries = unit.entries();
        while let Some(entry) = entries.next_dfs()? {
            if entry.tag() != gimli::DW_TAG_variable {
                continue;
            }
            let key = DieKey {
                unit: unit_index,
                offset: entry.offset().0,
            };
            let current_scope = scopes_by_die.get(&key).cloned().unwrap_or_default();
            if current_scope.routine {
                continue;
            }

            let (chain, chain_error) = match origin_chain(units, unit_index, entry) {
                Ok(chain) => (chain, None),
                Err(error) => (Vec::new(), Some(Arc::from(error.to_string()))),
            };
            let name = match copy_name_with_origins(dwarf, units, unit, entry, &chain) {
                Ok(Some(name)) => name,
                Ok(None) => format!("<anonymous global at {:#x}>", entry.offset().0).into(),
                Err(error) => {
                    format!("<malformed global at {:#x}: {error}>", entry.offset().0).into()
                }
            };
            // A malformed linkage name is an entry-local defect, not a reason to
            // discard the whole module's debug info. Fold any error into the
            // per-entry `malformed` state alongside declaration and chain errors.
            let linkage_result = copy_string_attribute_with_origins(
                dwarf,
                units,
                unit,
                entry,
                &chain,
                gimli::DW_AT_linkage_name,
            )
            .and_then(|primary| {
                primary.map_or_else(
                    || {
                        copy_string_attribute_with_origins(
                            dwarf,
                            units,
                            unit,
                            entry,
                            &chain,
                            gimli::DW_AT_MIPS_linkage_name,
                        )
                    },
                    |name| Ok(Some(name)),
                )
            });
            let (linkage_name, linkage_error) = match linkage_result {
                Ok(name) => (name, None),
                Err(error) => (None, Some(Arc::<str>::from(error.to_string()))),
            };
            let scope = chain
                .iter()
                .filter_map(|(origin_unit, origin)| {
                    scopes_by_die.get(&DieKey {
                        unit: *origin_unit,
                        offset: origin.offset().0,
                    })
                })
                .chain(std::iter::once(&current_scope))
                .max_by_key(|scope| scope.path.len())
                .cloned()
                .unwrap_or_default();
            let qualified_name = if scope.path.is_empty() {
                linkage_name
                    .as_ref()
                    .filter(|linkage| !linkage.starts_with('_') && linkage.contains('.'))
                    .cloned()
                    .unwrap_or_else(|| Arc::clone(&name))
            } else {
                Arc::from(format!(
                    "{}::{name}",
                    scope
                        .path
                        .iter()
                        .map(AsRef::as_ref)
                        .collect::<Vec<_>>()
                        .join("::")
                ))
            };
            let declaration = declaration_with_origins(
                dwarf,
                units,
                unit,
                entry,
                &chain,
                source_files,
                source_file_ids,
            );
            let (type_unit, type_value) = entry
                .attr_value(gimli::DW_AT_type)
                .map(|value| (unit_index, Some(value)))
                .or_else(|| {
                    chain.iter().find_map(|(origin_unit, origin)| {
                        origin
                            .attr_value(gimli::DW_AT_type)
                            .map(|value| (*origin_unit, Some(value)))
                    })
                })
                .unwrap_or((unit_index, None));
            let type_info = types.variable_type(type_unit, type_value);
            let value =
                copy_data_object_value_with_origins(dwarf, units, unit_index, unit, entry, &chain);
            let declaration_only =
                flag_attribute_with_origins(unit, entry, units, &chain, gimli::DW_AT_declaration)
                    .unwrap_or(false)
                    && matches!(value, Metadata::Unavailable(_));
            if declaration_only {
                continue;
            }
            let visibility =
                if flag_attribute_with_origins(unit, entry, units, &chain, gimli::DW_AT_external)
                    .unwrap_or(false)
                {
                    GlobalVariableVisibility::External
                } else {
                    GlobalVariableVisibility::CompilationUnit
                };
            *order = order
                .checked_add(1)
                .expect("data-object DIE order overflow");
            let malformed = declaration
                .as_ref()
                .err()
                .map(|error| Arc::from(error.to_string()))
                .or(chain_error)
                .or(linkage_error);
            let object = CatalogDataObject {
                debug_info_offset: entry
                    .offset()
                    .to_debug_info_offset(&unit.header)
                    .map(|offset| u64::try_from(offset.0).expect("DWARF offset fits u64")),
                kind: VariableKind::Global,
                name: Arc::clone(&name),
                declaration: declaration.as_ref().ok().cloned().flatten(),
                ranges: Vec::new().into(),
                instance: None,
                lexical_depth: 0,
                order: *order,
                type_info: type_info.clone(),
                value,
                frame_base: Metadata::Unavailable("globals have no frame base".into()),
                malformed,
            };
            let public_type = match &type_info {
                TypeResolution::Resolved(id) => match types
                    .entries
                    .get(usize::try_from(id.get()).expect("type ID fits usize"))
                {
                    Some(TypeEntry::Resolved(value)) => GlobalVariableType::Resolved(value.clone()),
                    Some(TypeEntry::Malformed(description)) => {
                        GlobalVariableType::Malformed(VariableMalformedReason {
                            description: Arc::clone(description),
                        })
                    }
                    Some(TypeEntry::Building) | None => {
                        GlobalVariableType::Malformed(VariableMalformedReason {
                            description: "type graph did not finish building".into(),
                        })
                    }
                },
                TypeResolution::Malformed(description) => {
                    GlobalVariableType::Malformed(VariableMalformedReason {
                        description: Arc::clone(description),
                    })
                }
            };
            let info = GlobalVariableInfo {
                id: GlobalVariableId::new(
                    u32::try_from(globals.len()).expect("global count fits u32"),
                ),
                name,
                qualified_name,
                linkage_name: linkage_name.clone(),
                declaration: declaration.ok().flatten(),
                type_info: public_type,
                visibility,
            };
            let canonical_die = chain.first().map_or(key, |(origin_unit, origin)| DieKey {
                unit: *origin_unit,
                offset: origin.offset().0,
            });
            let mut identities = vec![Arc::from(format!(
                "die:{}:{:#x}",
                canonical_die.unit, canonical_die.offset
            ))];
            if let Some(linkage_name) = &linkage_name {
                identities.push(Arc::from(format!("linkage:{linkage_name}")));
            }
            if let DefinitionResolution::Existing(existing_global) =
                definitions.resolve(&identities, globals.len())
            {
                let existing_object = global_objects[existing_global];
                if value_rank(&object.value) > value_rank(&objects[existing_object].value) {
                    objects[existing_object] = object;
                    globals[existing_global] = GlobalVariableInfo {
                        id: globals[existing_global].id,
                        ..info
                    };
                }
                continue;
            }
            global_objects.push(objects.len());
            objects.push(object);
            globals.push(info);
        }
    }

    Ok((globals, global_objects))
}

const fn value_rank(value: &Metadata<ValueDescription>) -> u8 {
    match value {
        Metadata::Value(ValueDescription::Location(_)) => 3,
        Metadata::Value(ValueDescription::Constant(_)) => 2,
        Metadata::Unavailable(_) => 1,
        Metadata::Malformed(_) => 0,
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one depth-first DIE walk must keep scope, variable, and parameter state synchronized"
)]
pub(super) fn load_variable_info<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    units: &[gimli::Unit<Reader<'data>>],
    target: TargetDescription,
    image_id: ModuleImageId,
    instance_ids: &HashMap<DieKey, CodeInstanceId>,
    source_files: &mut Vec<SourceFile>,
    source_file_ids: &mut HashMap<PathBuf, SourceFileId>,
) -> std::result::Result<LoadedVariables, DwarfError> {
    let mut objects = Vec::new();
    let mut functions = Vec::new();
    let mut order = 0_u64;
    let evaluation_units = load_evaluation_units(units)?;
    let mut types = TypeArenaBuilder::new(dwarf, units, image_id);
    let (globals, global_objects) = load_globals(
        dwarf,
        units,
        &mut objects,
        &mut order,
        source_files,
        source_file_ids,
        &mut types,
    )?;

    for (unit_index, unit) in units.iter().enumerate() {
        let mut entries = unit.entries();
        let mut scopes = Vec::<Option<Scope>>::new();

        while let Some(entry) = entries.next_dfs()? {
            let depth =
                usize::try_from(entry.depth()).map_err(|_| DwarfError::InvalidEntryDepth)?;
            scopes.truncate(depth);
            let parent = scopes.last().and_then(Clone::clone);

            let scope = match entry.tag() {
                gimli::DW_TAG_subprogram => {
                    let ranges = copy_ranges(dwarf, unit, entry)?;
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
                        ),
                        routine: true,
                        function,
                        instance: None,
                        malformed: None,
                    })
                }
                gimli::DW_TAG_lexical_block => parent.as_ref().map(|parent| {
                    let (ranges, malformed) = match copy_ranges(dwarf, unit, entry) {
                        Ok(ranges) if !ranges.is_empty() => (ranges, None),
                        Ok(_) => (Arc::clone(&parent.ranges), None),
                        Err(error) => (Arc::clone(&parent.ranges), Some(error.to_string().into())),
                    };
                    Scope {
                        ranges,
                        lexical_depth: parent.lexical_depth.saturating_add(1),
                        frame_base: parent.frame_base.clone(),
                        routine: false,
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
                    let (ranges, malformed) = match copy_ranges(dwarf, unit, entry) {
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
            types: types.entries.into(),
            objects_by_debug_offset,
            target,
            endian: match target.byte_order {
                ByteOrder::Little => RunTimeEndian::Little,
                ByteOrder::Big => RunTimeEndian::Big,
            },
        }),
        globals,
    })
}

fn data_object_scope_ranges(
    scope: &Scope,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
) -> (Arc<[AddressRange<ImageAddress>]>, Option<Arc<str>>) {
    let Some(attribute) = entry.attr(gimli::DW_AT_start_scope) else {
        return (Arc::clone(&scope.ranges), None);
    };
    let Some(offset) = attribute.udata_value() else {
        return (
            Arc::clone(&scope.ranges),
            Some("unsupported DW_AT_start_scope form".into()),
        );
    };
    let Some(first) = scope.ranges.first() else {
        return (Arc::clone(&scope.ranges), None);
    };
    let Some(start) = first.start.get().checked_add(offset) else {
        return (
            Arc::clone(&scope.ranges),
            Some("DW_AT_start_scope address overflow".into()),
        );
    };
    let ranges = scope
        .ranges
        .iter()
        .filter_map(|range| {
            let range_start = range.start.get().max(start);
            (range_start < range.end.get()).then_some(AddressRange {
                start: ImageAddress::new(range_start),
                end: range.end,
            })
        })
        .collect::<Vec<_>>()
        .into();
    (ranges, None)
}

const fn is_type_scope(tag: gimli::DwTag) -> bool {
    matches!(
        tag,
        gimli::DW_TAG_array_type
            | gimli::DW_TAG_base_type
            | gimli::DW_TAG_class_type
            | gimli::DW_TAG_enumeration_type
            | gimli::DW_TAG_pointer_type
            | gimli::DW_TAG_structure_type
            | gimli::DW_TAG_subroutine_type
            | gimli::DW_TAG_typedef
            | gimli::DW_TAG_union_type
    )
}

fn copy_name(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
) -> std::result::Result<Option<Arc<str>>, DwarfError> {
    entry
        .attr_value(gimli::DW_AT_name)
        .map(|value| dwarf.attr_string(unit, value))
        .transpose()
        .map_err(DwarfError::from)
        .map(|value| value.map(|value| Arc::from(value.to_string_lossy().into_owned())))
}

fn copy_string_attribute_with_origins(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    units: &[gimli::Unit<Reader<'_>>],
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    chain: &[(usize, gimli::DebuggingInformationEntry<Reader<'_>>)],
    attribute: gimli::DwAt,
) -> std::result::Result<Option<Arc<str>>, DwarfError> {
    if let Some(value) = entry.attr_value(attribute) {
        return Ok(Some(Arc::from(
            dwarf
                .attr_string(unit, value)?
                .to_string_lossy()
                .into_owned(),
        )));
    }
    for (origin_unit, origin) in chain {
        if let Some(value) = origin.attr_value(attribute) {
            return Ok(Some(Arc::from(
                dwarf
                    .attr_string(&units[*origin_unit], value)?
                    .to_string_lossy()
                    .into_owned(),
            )));
        }
    }
    Ok(None)
}

fn flag_attribute_with_origins(
    _unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    _units: &[gimli::Unit<Reader<'_>>],
    chain: &[(usize, gimli::DebuggingInformationEntry<Reader<'_>>)],
    attribute: gimli::DwAt,
) -> Option<bool> {
    let flag = |attribute: &gimli::Attribute<Reader<'_>>| match attribute.value() {
        gimli::AttributeValue::Flag(value) => Some(value),
        _ => None,
    };
    entry.attr(attribute).and_then(flag).or_else(|| {
        chain
            .iter()
            .find_map(|(_, origin)| origin.attr(attribute).and_then(flag))
    })
}

/// Follows `DW_AT_abstract_origin`/`DW_AT_specification` references
/// transitively, rejecting cycles, so concrete inline-instance DIEs can
/// inherit name, type, and declaration metadata from their origins.
fn origin_chain<'data>(
    units: &[gimli::Unit<Reader<'data>>],
    unit_index: usize,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
) -> std::result::Result<Vec<(usize, gimli::DebuggingInformationEntry<Reader<'data>>)>, DwarfError>
{
    let keys = checked_reference_chain(origin_reference(entry, unit_index, units)?, |key| {
        let unit = units
            .get(key.unit)
            .ok_or(DwarfError::ReferenceOutsideUnits(key.offset))?;
        let origin = unit.entry(gimli::UnitOffset(key.offset))?;
        origin_reference(&origin, key.unit, units)
    })?;
    let mut chain = Vec::with_capacity(keys.len());
    for key in keys {
        let unit = units
            .get(key.unit)
            .ok_or(DwarfError::ReferenceOutsideUnits(key.offset))?;
        chain.push((key.unit, unit.entry(gimli::UnitOffset(key.offset))?));
    }
    Ok(chain)
}

fn checked_reference_chain(
    mut current: Option<DieKey>,
    mut next: impl FnMut(DieKey) -> std::result::Result<Option<DieKey>, DwarfError>,
) -> std::result::Result<Vec<DieKey>, DwarfError> {
    let mut chain = Vec::new();
    let mut visited = HashSet::new();
    while let Some(key) = current {
        if !visited.insert(key) {
            return Err(DwarfError::ReferenceCycle);
        }
        current = next(key)?;
        chain.push(key);
    }
    Ok(chain)
}

fn origin_reference(
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    unit_index: usize,
    units: &[gimli::Unit<Reader<'_>>],
) -> std::result::Result<Option<DieKey>, DwarfError> {
    let value = entry
        .attr_value(gimli::DW_AT_abstract_origin)
        .or_else(|| entry.attr_value(gimli::DW_AT_specification));
    die_reference(value, unit_index, units)
}

fn copy_name_with_origins(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    units: &[gimli::Unit<Reader<'_>>],
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    chain: &[(usize, gimli::DebuggingInformationEntry<Reader<'_>>)],
) -> std::result::Result<Option<Arc<str>>, DwarfError> {
    if let Some(name) = copy_name(dwarf, unit, entry)? {
        return Ok(Some(name));
    }
    for (origin_unit, origin_entry) in chain {
        if let Some(name) = copy_name(dwarf, &units[*origin_unit], origin_entry)? {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

fn declaration_with_origins<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    units: &[gimli::Unit<Reader<'data>>],
    unit: &gimli::Unit<Reader<'data>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
    chain: &[(usize, gimli::DebuggingInformationEntry<Reader<'data>>)],
    source_files: &mut Vec<SourceFile>,
    source_file_ids: &mut HashMap<PathBuf, SourceFileId>,
) -> std::result::Result<Option<SourceLocation>, DwarfError> {
    // DWARF inherits declaration attributes individually: each of decl_file,
    // decl_line, and decl_column comes from the first DIE in the chain that
    // supplies it. decl_file indexes the line program of the unit that owns
    // the DIE supplying it.
    let mut dies = Vec::with_capacity(chain.len() + 1);
    dies.push((unit, entry));
    for (origin_unit, origin_entry) in chain {
        dies.push((&units[*origin_unit], origin_entry));
    }
    let file = dies.iter().find_map(|(unit, entry)| {
        entry
            .attr(gimli::DW_AT_decl_file)
            .and_then(gimli::Attribute::udata_value)
            .map(|index| (*unit, index))
    });
    let line = dies
        .iter()
        .find_map(|(_, entry)| {
            entry
                .attr(gimli::DW_AT_decl_line)
                .and_then(gimli::Attribute::udata_value)
        })
        .and_then(LineNumber::new);
    let (Some((file_unit, file_index)), Some(line)) = (file, line) else {
        return Ok(None);
    };
    let Some(program) = file_unit.line_program.as_ref() else {
        return Ok(None);
    };
    let Some(file) = program.header().file(file_index) else {
        return Ok(None);
    };
    let path = source_path(dwarf, file_unit, program.header(), file)?;
    Ok(Some(SourceLocation {
        file: source_file_id(path, source_files, source_file_ids),
        line,
        column: dies
            .iter()
            .find_map(|(_, entry)| {
                entry
                    .attr(gimli::DW_AT_decl_column)
                    .and_then(gimli::Attribute::udata_value)
            })
            .and_then(ColumnNumber::new),
    }))
}

fn copy_ranges<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    unit: &gimli::Unit<Reader<'data>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
) -> std::result::Result<Arc<[AddressRange<ImageAddress>]>, DwarfError> {
    let mut ranges = dwarf.die_ranges(unit, entry)?;
    let mut copied = Vec::new();
    while let Some(range) = ranges.next()? {
        if range.begin > range.end {
            return Err(DwarfError::InvalidRange);
        }
        if range.begin < range.end {
            copied.push(AddressRange {
                start: ImageAddress::new(range.begin),
                end: ImageAddress::new(range.end),
            });
        }
    }
    Ok(copied.into())
}

fn copy_optional_location(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    value: Option<gimli::AttributeValue<Reader<'_>>>,
) -> Metadata<LocationDescription> {
    let Some(value) = value else {
        return Metadata::Unavailable("no location was supplied".into());
    };
    match copy_location(dwarf, unit_index, unit, value) {
        Ok(location) => Metadata::Value(location),
        Err(error) => Metadata::Malformed(error.to_string().into()),
    }
}

fn copy_data_object_value(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
) -> Metadata<ValueDescription> {
    if let Some(location) = entry.attr_value(gimli::DW_AT_location) {
        return match copy_optional_location(dwarf, unit_index, unit, Some(location)) {
            Metadata::Value(location) => Metadata::Value(ValueDescription::Location(location)),
            Metadata::Unavailable(reason) => Metadata::Unavailable(reason),
            Metadata::Malformed(reason) => Metadata::Malformed(reason),
        };
    }
    if let Some(value) = entry.attr_value(gimli::DW_AT_const_value) {
        return match copy_constant(value) {
            Ok(value) => Metadata::Value(ValueDescription::Constant(value)),
            Err(error) => Metadata::Malformed(error),
        };
    }
    Metadata::Unavailable("no location was supplied".into())
}

fn copy_data_object_value_with_origins(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    units: &[gimli::Unit<Reader<'_>>],
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    chain: &[(usize, gimli::DebuggingInformationEntry<Reader<'_>>)],
) -> Metadata<ValueDescription> {
    let direct = copy_data_object_value(dwarf, unit_index, unit, entry);
    if !matches!(direct, Metadata::Unavailable(_)) {
        return direct;
    }
    for (origin_unit, origin) in chain {
        let inherited = copy_data_object_value(dwarf, *origin_unit, &units[*origin_unit], origin);
        if !matches!(inherited, Metadata::Unavailable(_)) {
            return inherited;
        }
    }
    direct
}

fn copy_constant(
    value: gimli::AttributeValue<Reader<'_>>,
) -> std::result::Result<ConstantValue, Arc<str>> {
    Ok(match value {
        // Fixed-width forms carry raw bits; signedness comes from the type.
        gimli::AttributeValue::Data1(value) => ConstantValue::Fixed(u128::from(value)),
        gimli::AttributeValue::Data2(value) => ConstantValue::Fixed(u128::from(value)),
        gimli::AttributeValue::Data4(value) => ConstantValue::Fixed(u128::from(value)),
        gimli::AttributeValue::Data8(value) => ConstantValue::Fixed(u128::from(value)),
        gimli::AttributeValue::Data16(value) => ConstantValue::Fixed(value),
        gimli::AttributeValue::Udata(value) => ConstantValue::Unsigned(u128::from(value)),
        gimli::AttributeValue::Sdata(value) => ConstantValue::Signed(i128::from(value)),
        gimli::AttributeValue::Block(value) => ConstantValue::Bytes(Arc::from(
            value
                .to_slice()
                .map_err(|error| Arc::from(error.to_string()))?
                .into_owned(),
        )),
        _ => return Err("unsupported DW_AT_const_value form".into()),
    })
}

fn copy_location(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    value: gimli::AttributeValue<Reader<'_>>,
) -> std::result::Result<LocationDescription, DwarfError> {
    let encoding = unit.encoding();
    if let gimli::AttributeValue::Exprloc(expression) = value {
        return Ok(LocationDescription {
            entries: vec![LocationEntry {
                range: None,
                expression: copy_expression(dwarf, unit_index, unit, expression, encoding)?,
            }]
            .into(),
        });
    }
    let mut locations = dwarf
        .attr_locations(unit, value)?
        .ok_or(DwarfError::UnsupportedReferenceForm)?;
    let mut entries = Vec::new();
    // Iterate raw entries so DW_LLE_default_location keeps its fallback
    // semantics (range: None) instead of becoming a 0..u64::MAX range that
    // conflicts with every specific entry.
    while let Some(raw) = locations.next_raw()? {
        let is_default = matches!(&raw, gimli::RawLocListEntry::DefaultLocation { .. });
        let Some(location) = locations.convert_raw(raw)? else {
            continue;
        };
        if is_default {
            entries.push(LocationEntry {
                range: None,
                expression: copy_expression(dwarf, unit_index, unit, location.data, encoding)?,
            });
            continue;
        }
        if location.range.begin > location.range.end {
            return Err(DwarfError::InvalidRange);
        }
        if location.range.begin < location.range.end {
            entries.push(LocationEntry {
                range: Some(AddressRange {
                    start: ImageAddress::new(location.range.begin),
                    end: ImageAddress::new(location.range.end),
                }),
                expression: copy_expression(dwarf, unit_index, unit, location.data, encoding)?,
            });
        }
    }
    Ok(LocationDescription {
        entries: entries.into(),
    })
}

fn copy_expression(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'_>>,
    expression: gimli::Expression<Reader<'_>>,
    encoding: gimli::Encoding,
) -> std::result::Result<Expression, DwarfError> {
    let mut indexed_addresses = HashMap::new();
    let mut operations = expression.operations(encoding);
    while let Some(operation) = operations.next()? {
        let (gimli::Operation::AddressIndex { index } | gimli::Operation::ConstantIndex { index }) =
            operation
        else {
            continue;
        };
        let address = dwarf.address(unit, index)?;
        indexed_addresses.insert(index.0, address);
    }
    let bytes: Cow<'_, [u8]> = expression.0.to_slice()?;
    Ok(Expression {
        bytes: Arc::from(bytes.into_owned()),
        encoding,
        unit: unit_index,
        indexed_addresses: Arc::new(indexed_addresses),
    })
}

fn load_evaluation_units(
    units: &[gimli::Unit<Reader<'_>>],
) -> std::result::Result<Vec<EvaluationUnit>, DwarfError> {
    units
        .iter()
        .map(|unit| {
            let mut base_types = HashMap::new();
            let mut entries = unit.entries();
            while let Some(entry) = entries.next_dfs()? {
                if entry.tag() != gimli::DW_TAG_base_type {
                    continue;
                }
                let Some(byte_size) = entry
                    .attr(gimli::DW_AT_byte_size)
                    .and_then(gimli::Attribute::udata_value)
                else {
                    continue;
                };
                let Some(raw_encoding) = entry
                    .attr(gimli::DW_AT_encoding)
                    .and_then(gimli::Attribute::udata_value)
                else {
                    continue;
                };
                let encoding = gimli::DwAte(u8::try_from(raw_encoding).unwrap_or(u8::MAX));
                if let Some(value_type) = dwarf_value_type(encoding, byte_size) {
                    base_types.insert(entry.offset().0, value_type);
                }
            }
            Ok(EvaluationUnit { base_types })
        })
        .collect()
}

const fn dwarf_value_type(encoding: gimli::DwAte, byte_size: u64) -> Option<gimli::ValueType> {
    use gimli::ValueType::{F32, F64, I8, I16, I32, I64, U8, U16, U32, U64};
    if encoding.0 == gimli::DW_ATE_float.0 {
        return match byte_size {
            4 => Some(F32),
            8 => Some(F64),
            _ => None,
        };
    }
    let signed = encoding.0 == gimli::DW_ATE_signed.0 || encoding.0 == gimli::DW_ATE_signed_char.0;
    let unsigned = encoding.0 == gimli::DW_ATE_boolean.0
        || encoding.0 == gimli::DW_ATE_unsigned.0
        || encoding.0 == gimli::DW_ATE_unsigned_char.0;
    match (signed, unsigned, byte_size) {
        (true, false, 1) => Some(I8),
        (true, false, 2) => Some(I16),
        (true, false, 4) => Some(I32),
        (true, false, 8) => Some(I64),
        (false, true, 1) => Some(U8),
        (false, true, 2) => Some(U16),
        (false, true, 4) => Some(U32),
        (false, true, 8) => Some(U64),
        _ => None,
    }
}

impl<'a, 'data> TypeArenaBuilder<'a, 'data> {
    fn new(
        dwarf: &'a gimli::Dwarf<Reader<'data>>,
        units: &'a [gimli::Unit<Reader<'data>>],
        image: ModuleImageId,
    ) -> Self {
        Self {
            dwarf,
            units,
            image,
            by_die: HashMap::new(),
            entries: Vec::new(),
        }
    }

    fn variable_type(
        &mut self,
        unit_index: usize,
        value: Option<gimli::AttributeValue<Reader<'data>>>,
    ) -> TypeResolution {
        let key = match die_reference(value, unit_index, self.units) {
            Ok(Some(key)) => key,
            Ok(None) => return TypeResolution::Malformed("variable has no type".into()),
            Err(error) => return TypeResolution::Malformed(error.to_string().into()),
        };
        let id = self.resolve(key);
        match self
            .entries
            .get(usize::try_from(id.get()).expect("type ID fits usize"))
        {
            Some(TypeEntry::Malformed(reason)) => TypeResolution::Malformed(Arc::clone(reason)),
            Some(TypeEntry::Building) => {
                TypeResolution::Malformed("type graph did not finish building".into())
            }
            Some(TypeEntry::Resolved(_)) => TypeResolution::Resolved(id),
            None => TypeResolution::Malformed("type ID is outside the arena".into()),
        }
    }

    fn resolve(&mut self, key: DieKey) -> TypeId {
        if let Some(id) = self.by_die.get(&key) {
            return *id;
        }
        let id = TypeId::new(u32::try_from(self.entries.len()).expect("type count fits u32"));
        self.by_die.insert(key, id);
        self.entries.push(TypeEntry::Building);
        let entry = self.build(key, id);
        self.entries[usize::try_from(id.get()).expect("type ID fits usize")] = entry;
        id
    }

    fn build(&mut self, key: DieKey, id: TypeId) -> TypeEntry {
        let Some(unit) = self.units.get(key.unit) else {
            return TypeEntry::Malformed("type reference is outside loaded units".into());
        };
        let entry = match unit.entry(gimli::UnitOffset(key.offset)) {
            Ok(entry) => entry,
            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
        };
        let reference = TypeReference {
            image: self.image,
            id,
        };
        let explicit_name = match copy_name(self.dwarf, unit, &entry) {
            Ok(name) => name,
            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
        };
        let explicit_size = entry
            .attr(gimli::DW_AT_byte_size)
            .and_then(gimli::Attribute::udata_value);
        let address_class = entry
            .attr(gimli::DW_AT_address_class)
            .and_then(gimli::Attribute::udata_value)
            .unwrap_or(0);
        let pointer_size = explicit_size
            .or_else(|| (address_class == 0).then_some(u64::from(unit.encoding().address_size)));

        match entry.tag() {
            gimli::DW_TAG_base_type => {
                Self::build_base_type(&entry, reference, explicit_name, explicit_size)
            }
            gimli::DW_TAG_pointer_type => self.build_pointer_type(
                &entry,
                key.unit,
                reference,
                explicit_name,
                pointer_size,
                address_class,
            ),
            gimli::DW_TAG_reference_type | gimli::DW_TAG_rvalue_reference_type => self
                .build_reference_type(
                    &entry,
                    key.unit,
                    reference,
                    explicit_name,
                    pointer_size,
                    address_class,
                ),
            gimli::DW_TAG_typedef
            | gimli::DW_TAG_const_type
            | gimli::DW_TAG_volatile_type
            | gimli::DW_TAG_restrict_type
            | gimli::DW_TAG_atomic_type
            | gimli::DW_TAG_immutable_type => {
                self.build_wrapper_type(&entry, key.unit, reference, explicit_name, explicit_size)
            }
            gimli::DW_TAG_unspecified_type => TypeEntry::Resolved(TypeInfo {
                reference,
                name: explicit_name.unwrap_or_else(|| Arc::from("void")),
                byte_size: explicit_size,
                kind: TypeKind::Unspecified,
            }),
            tag => TypeEntry::Resolved(TypeInfo {
                reference,
                name: explicit_name.unwrap_or_else(|| Arc::from(format!("{tag:?}"))),
                byte_size: explicit_size,
                kind: TypeKind::Opaque {
                    description: format!("type tag {tag:?} is unsupported").into(),
                },
            }),
        }
    }

    fn build_base_type(
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
    ) -> TypeEntry {
        let name = explicit_name.unwrap_or_else(|| Arc::from("<unnamed base type>"));
        let Some(byte_size) = explicit_size else {
            return TypeEntry::Malformed("base type has no byte size".into());
        };
        let Some(raw_encoding) = entry
            .attr(gimli::DW_AT_encoding)
            .and_then(gimli::Attribute::udata_value)
        else {
            return TypeEntry::Malformed("base type has no encoding".into());
        };
        let encoding = match gimli::DwAte(u8::try_from(raw_encoding).unwrap_or(u8::MAX)) {
            gimli::DW_ATE_boolean => BaseTypeEncoding::Boolean,
            gimli::DW_ATE_signed => BaseTypeEncoding::Signed,
            gimli::DW_ATE_signed_char => BaseTypeEncoding::SignedCharacter,
            gimli::DW_ATE_unsigned => BaseTypeEncoding::Unsigned,
            gimli::DW_ATE_unsigned_char => BaseTypeEncoding::UnsignedCharacter,
            gimli::DW_ATE_float => BaseTypeEncoding::Floating,
            other => {
                return TypeEntry::Resolved(TypeInfo {
                    reference,
                    name,
                    byte_size: Some(byte_size),
                    kind: TypeKind::Opaque {
                        description: format!("base type encoding {other:?} is unsupported").into(),
                    },
                });
            }
        };
        let base = BaseType {
            name: Arc::clone(&name),
            base_name: Arc::clone(&name),
            encoding,
            byte_size,
        };
        TypeEntry::Resolved(TypeInfo {
            reference,
            name,
            byte_size: Some(byte_size),
            kind: TypeKind::Base(base),
        })
    }

    fn target(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> std::result::Result<Option<TypeReference>, Arc<str>> {
        die_reference(entry.attr_value(gimli::DW_AT_type), unit_index, self.units)
            .map(|key| {
                key.map(|key| TypeReference {
                    image: self.image,
                    id: self.resolve(key),
                })
            })
            .map_err(|error| error.to_string().into())
    }

    fn target_name(&self, target: TypeReference) -> Arc<str> {
        self.entries
            .get(usize::try_from(target.id.get()).expect("type ID fits usize"))
            .and_then(|entry| match entry {
                TypeEntry::Resolved(info) => Some(Arc::clone(&info.name)),
                TypeEntry::Building | TypeEntry::Malformed(_) => None,
            })
            .unwrap_or_else(|| Arc::from("<recursive type>"))
    }

    fn build_pointer_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        byte_size: Option<u64>,
        address_class: u64,
    ) -> TypeEntry {
        let target = match self.target(entry, unit_index) {
            Ok(target) => target,
            Err(reason) => return TypeEntry::Malformed(reason),
        };
        let name = explicit_name.unwrap_or_else(|| {
            target.map_or_else(
                || Arc::from("void *"),
                |target| Arc::from(format!("{} *", self.target_name(target))),
            )
        });
        TypeEntry::Resolved(TypeInfo {
            reference,
            name,
            byte_size,
            kind: TypeKind::Pointer {
                target,
                address_class,
            },
        })
    }

    fn build_reference_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        byte_size: Option<u64>,
        address_class: u64,
    ) -> TypeEntry {
        let target = match self.target(entry, unit_index) {
            Ok(Some(target)) => target,
            Ok(None) => return TypeEntry::Malformed("reference type has no target".into()),
            Err(reason) => return TypeEntry::Malformed(reason),
        };
        let kind = if entry.tag() == gimli::DW_TAG_reference_type {
            ReferenceKind::Lvalue
        } else {
            ReferenceKind::Rvalue
        };
        let suffix = if kind == ReferenceKind::Lvalue {
            "&"
        } else {
            "&&"
        };
        let name = explicit_name
            .unwrap_or_else(|| Arc::from(format!("{} {suffix}", self.target_name(target))));
        TypeEntry::Resolved(TypeInfo {
            reference,
            name,
            byte_size,
            kind: TypeKind::Reference {
                kind,
                target,
                address_class,
            },
        })
    }

    fn build_wrapper_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
    ) -> TypeEntry {
        let target = match self.target(entry, unit_index) {
            Ok(Some(target)) => target,
            Ok(None) => return TypeEntry::Malformed("type wrapper has no target".into()),
            Err(reason) => return TypeEntry::Malformed(reason),
        };
        let inherited_size = self
            .entries
            .get(usize::try_from(target.id.get()).expect("type ID fits usize"))
            .and_then(|entry| match entry {
                TypeEntry::Resolved(info) => info.byte_size,
                TypeEntry::Building | TypeEntry::Malformed(_) => None,
            });
        let byte_size = explicit_size.or(inherited_size);
        if entry.tag() == gimli::DW_TAG_typedef {
            return TypeEntry::Resolved(TypeInfo {
                reference,
                name: explicit_name.unwrap_or_else(|| self.target_name(target)),
                byte_size,
                kind: TypeKind::Alias { target },
            });
        }
        let qualifier = match entry.tag() {
            gimli::DW_TAG_const_type => TypeQualifier::Const,
            gimli::DW_TAG_volatile_type => TypeQualifier::Volatile,
            gimli::DW_TAG_restrict_type => TypeQualifier::Restrict,
            gimli::DW_TAG_atomic_type => TypeQualifier::Atomic,
            gimli::DW_TAG_immutable_type => TypeQualifier::Immutable,
            _ => unreachable!("wrapper tags matched by caller"),
        };
        let name = explicit_name
            .unwrap_or_else(|| Arc::from(format!("{qualifier:?} {}", self.target_name(target))));
        TypeEntry::Resolved(TypeInfo {
            reference,
            name,
            byte_size,
            kind: TypeKind::Qualified { qualifier, target },
        })
    }
}

impl VariableInfo for DwarfVariableInfo {
    fn inspect(
        &self,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        query: &VariableQuery,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
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
        // Source-level visibility is per logical frame: only data objects owned
        // by the selected inline instance (or the physical frame for `None`)
        // are in scope, even though siblings share the instruction address.
        let active = function
            .objects
            .iter()
            .map(|&index| &self.objects[index])
            .filter(|object| object.instance == selected)
            .filter(|object| object.ranges.iter().any(|range| range.contains(address)))
            .collect::<Vec<_>>();
        let selected_objects = match query {
            VariableQuery::All => active,
            VariableQuery::Name(name) => {
                let mut named = active
                    .into_iter()
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
        };
        let mut selected = selected_objects;
        selected.sort_by_key(|object| {
            if object.kind == VariableKind::Parameter {
                return (0, 0, SourceFileId::new(0), 0, 0, object.order);
            }
            object.declaration.as_ref().map_or(
                (
                    1,
                    1,
                    SourceFileId::new(u32::MAX),
                    u64::MAX,
                    u64::MAX,
                    object.order,
                ),
                |location| {
                    (
                        1,
                        0,
                        location.file,
                        location.line.get(),
                        location.column.map_or(0, crate::ColumnNumber::get),
                        object.order,
                    )
                },
            )
        });
        let mut frame_base = FrameBaseCache::Empty;
        Ok(selected
            .into_iter()
            .map(|object| {
                self.inspect_data_object(object, Some(address), context, runtime, &mut frame_base)
            })
            .collect())
    }

    fn inspect_global(
        &self,
        id: GlobalVariableId,
        address: Option<ImageAddress>,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
    ) -> Result<Variable> {
        let global_index = usize::try_from(id.get()).expect("u32 fits usize");
        let object_index = *self
            .globals
            .get(global_index)
            .ok_or_else(|| Error::VariableNotFound(id.to_string()))?;
        let object = &self.objects[object_index];
        let mut frame_base = FrameBaseCache::Empty;
        Ok(self.inspect_data_object(object, address, context, runtime, &mut frame_base))
    }

    fn dereference(
        &self,
        reference: &DereferenceReference,
        runtime: &mut dyn VariableRuntime,
    ) -> Result<DereferencedValue> {
        self.dereference_value(reference, runtime)
    }
}

fn type_info_from(types: &[TypeEntry], id: TypeId) -> std::result::Result<&TypeInfo, Arc<str>> {
    match types.get(usize::try_from(id.get()).expect("type ID fits usize")) {
        Some(TypeEntry::Resolved(info)) => Ok(info),
        Some(TypeEntry::Malformed(reason)) => Err(Arc::clone(reason)),
        Some(TypeEntry::Building) => Err("type graph did not finish building".into()),
        None => Err("type ID is outside the module arena".into()),
    }
}

fn value_shape_from(types: &[TypeEntry], id: TypeId) -> std::result::Result<ValueShape, Arc<str>> {
    let mut current = id;
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(current) {
            return Err("type wrapper cycle".into());
        }
        let info = type_info_from(types, current)?;
        match &info.kind {
            TypeKind::Base(base) => {
                if base.byte_size > MAX_SCALAR_BYTES {
                    return Err(format!("scalar type occupies {} bytes", base.byte_size).into());
                }
                let mut base = base.clone();
                base.name = Arc::clone(&type_info_from(types, id)?.name);
                return Ok(ValueShape::Scalar(base));
            }
            TypeKind::Pointer {
                target,
                address_class,
            } => {
                let byte_size = info
                    .byte_size
                    .ok_or_else(|| Arc::<str>::from("pointer type has no byte size"))?;
                return Ok(ValueShape::Indirection {
                    target: target.map(|target| target.id),
                    byte_size,
                    address_class: *address_class,
                });
            }
            TypeKind::Reference {
                target,
                address_class,
                ..
            } => {
                let byte_size = info
                    .byte_size
                    .ok_or_else(|| Arc::<str>::from("reference type has no byte size"))?;
                return Ok(ValueShape::Indirection {
                    target: Some(target.id),
                    byte_size,
                    address_class: *address_class,
                });
            }
            TypeKind::Qualified { target, .. } | TypeKind::Alias { target } => {
                current = target.id;
            }
            TypeKind::Unspecified => return Err("unspecified values are unsupported".into()),
            TypeKind::Opaque { description } => return Err(Arc::clone(description)),
        }
    }
}

impl DwarfVariableInfo {
    fn type_info(&self, id: TypeId) -> std::result::Result<&TypeInfo, Arc<str>> {
        type_info_from(&self.types, id)
    }

    fn value_shape(&self, id: TypeId) -> std::result::Result<ValueShape, Arc<str>> {
        value_shape_from(&self.types, id)
    }

    fn function_at(&self, address: ImageAddress) -> Option<&CatalogFunction> {
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

    #[expect(
        clippy::too_many_lines,
        reason = "the object inspection pipeline keeps every typed failure at its originating boundary"
    )]
    fn inspect_data_object(
        &self,
        variable: &CatalogDataObject,
        address: Option<ImageAddress>,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        frame_base_cache: &mut FrameBaseCache,
    ) -> Variable {
        if let Some(description) = &variable.malformed {
            return malformed(variable, None, Arc::clone(description));
        }
        let type_id = match &variable.type_info {
            TypeResolution::Resolved(id) => *id,
            TypeResolution::Malformed(description) => {
                return malformed(variable, None, Arc::clone(description));
            }
        };
        let type_info = match self.type_info(type_id) {
            Ok(info) => info.clone(),
            Err(description) => return malformed(variable, None, description),
        };
        let shape = match self.value_shape(type_id) {
            Ok(shape) => shape,
            Err(description) => {
                return unavailable(variable, Some(type_info), description.into());
            }
        };
        let description = match &variable.value {
            Metadata::Value(location) => location,
            Metadata::Unavailable(description) => {
                let reason = if description.as_ref() == "no location was supplied" {
                    VariableUnavailableReason::OptimizedOut
                } else {
                    Arc::clone(description).into()
                };
                return unavailable(variable, Some(type_info), reason);
            }
            Metadata::Malformed(description) => {
                return malformed(variable, Some(type_info), Arc::clone(description));
            }
        };
        if let ValueDescription::Constant(constant) = description {
            let raw = match materialize_constant(
                constant,
                usize::try_from(shape.byte_size()).expect("supported value size fits usize"),
                self.target,
            ) {
                Ok(raw) => raw,
                Err(reason) => return unavailable(variable, Some(type_info), reason),
            };
            return self.available_variable(
                variable,
                type_info,
                &shape,
                context,
                VariableValueSource::Constant,
                raw,
            );
        }
        let ValueDescription::Location(location) = description else {
            unreachable!("constant values returned above")
        };
        let expression = match location.expression(address) {
            Ok(Some(expression)) => expression,
            Err(reason) => return unavailable(variable, Some(type_info), reason),
            Ok(None) => {
                return unavailable(
                    variable,
                    Some(type_info),
                    "no location at the current instruction".into(),
                );
            }
        };
        let mut budget = EvaluationBudget::default();
        // The frame base resolves lazily so an unexecuted DW_OP_fbreg branch
        // cannot fail a variable whose executed path never needs it.
        let mut frame_base = FrameBase::Lazy(FrameBaseContext {
            location: &variable.frame_base,
            address,
            cache: frame_base_cache,
        });
        let pieces = match evaluate(
            expression,
            self.endian,
            &mut frame_base,
            &self.evaluation_units,
            runtime,
            &mut budget,
        ) {
            Ok(pieces) => pieces,
            Err(EvaluateError::Unavailable(reason)) => {
                return unavailable(variable, Some(type_info), reason);
            }
            Err(EvaluateError::Malformed(description)) => {
                return malformed(variable, Some(type_info), description);
            }
        };
        if let [piece] = pieces.as_slice()
            && let Location::ImplicitPointer { value, byte_offset } = piece.location
        {
            let mut value = available_implicit_pointer(
                variable,
                type_info,
                &shape,
                context,
                ImplicitPointerLocation {
                    debug_info_offset: u64::try_from(value.0).expect("DWARF offset fits u64"),
                    byte_offset,
                    size_in_bits: piece.size_in_bits,
                    bit_offset: piece.bit_offset,
                },
            );
            self.constrain_dereference(&mut value.state, &shape);
            return value;
        }
        let (source, raw) = match materialize_pieces(
            &pieces,
            shape.byte_size(),
            shape.scalar(),
            self.endian,
            self.target,
            runtime,
            &mut budget,
        ) {
            Ok(value) => value,
            Err(reason) => return unavailable(variable, Some(type_info), reason),
        };
        self.available_variable(variable, type_info, &shape, context, source, raw)
    }

    fn available_variable(
        &self,
        variable: &CatalogDataObject,
        type_info: TypeInfo,
        shape: &ValueShape,
        context: VariableContext,
        source: VariableValueSource,
        raw: Arc<[u8]>,
    ) -> Variable {
        let mut value = available(
            variable,
            type_info,
            shape,
            context,
            source,
            raw,
            self.target,
        );
        self.constrain_dereference(&mut value.state, shape);
        value
    }

    fn constrain_dereference(&self, state: &mut VariableState, shape: &ValueShape) {
        let ValueShape::Indirection {
            target: Some(target),
            ..
        } = shape
        else {
            return;
        };
        if !matches!(
            state,
            VariableState::Available {
                dereference: DereferenceState::Available(_),
                ..
            }
        ) {
            return;
        }
        let unavailable = match self.type_info(*target) {
            Err(description) => Some(DereferenceUnavailableReason::Malformed(
                VariableMalformedReason { description },
            )),
            Ok(TypeInfo {
                kind: TypeKind::Unspecified,
                ..
            }) => Some(DereferenceUnavailableReason::UnspecifiedPointee),
            Ok(_) => self
                .value_shape(*target)
                .err()
                .map(DereferenceUnavailableReason::UnsupportedPointee),
        };
        if let Some(reason) = unavailable
            && let VariableState::Available { dereference, .. } = state
        {
            *dereference = DereferenceState::Unavailable(reason);
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "dereference preserves distinct address, implicit-pointer, unavailable, and malformed outcomes"
    )]
    fn dereference_value(
        &self,
        reference: &DereferenceReference,
        runtime: &mut dyn VariableRuntime,
    ) -> Result<DereferencedValue> {
        let type_info = self
            .type_info(reference.target_type)
            .map_err(|reason| Error::debug_info(DwarfError::MalformedVariable(reason)))?
            .clone();
        let shape = match self.value_shape(reference.target_type) {
            Ok(shape) => shape,
            Err(reason) => {
                return Ok(DereferencedValue {
                    type_info,
                    state: VariableState::Unavailable(reason.into()),
                });
            }
        };
        let context = VariableContext {
            stop_id: reference.stop_id,
            thread: reference.thread,
            module: reference.module,
            image: reference.image,
            address: reference.context_address,
        };
        let raw = match reference.target {
            crate::model::DereferenceTarget::Address(address) => {
                let size = usize::try_from(shape.byte_size())
                    .map_err(|_| Error::debug_info(DwarfError::InvalidRange))?;
                if size > MAX_EVALUATION_MEMORY_BYTES {
                    return Ok(DereferencedValue {
                        type_info,
                        state: VariableState::Unavailable(
                            VariableUnavailableReason::EvaluationLimit,
                        ),
                    });
                }
                match runtime.read_memory(address, size) {
                    Ok(raw) => (VariableValueSource::Memory(address), raw),
                    Err(reason) => {
                        return Ok(DereferencedValue {
                            type_info,
                            state: VariableState::Unavailable(VariableUnavailableReason::Other(
                                reason,
                            )),
                        });
                    }
                }
            }
            crate::model::DereferenceTarget::ImplicitPointer {
                debug_info_offset,
                byte_offset,
            } => {
                let Some(object_index) = self
                    .objects_by_debug_offset
                    .get(&debug_info_offset)
                    .copied()
                else {
                    return Ok(DereferencedValue {
                        type_info,
                        state: VariableState::Unavailable(
                            crate::UnsupportedVariableFeature::CrossDieEvaluation.into(),
                        ),
                    });
                };
                let object = &self.objects[object_index];
                let mut frame_base = FrameBaseCache::Empty;
                let referenced = self.inspect_data_object(
                    object,
                    reference.context_address,
                    context,
                    runtime,
                    &mut frame_base,
                );
                if byte_offset == 0
                    && matches!(object.type_info, TypeResolution::Resolved(id) if id == reference.target_type)
                {
                    return Ok(DereferencedValue {
                        type_info,
                        state: referenced.state,
                    });
                }
                let raw = match referenced.state {
                    VariableState::Available { raw: Some(raw), .. } => raw,
                    VariableState::Available { raw: None, .. } => {
                        return Ok(DereferencedValue {
                            type_info,
                            state: VariableState::Unavailable(
                                crate::UnsupportedVariableFeature::CompositeLocation.into(),
                            ),
                        });
                    }
                    VariableState::Unavailable(reason) => {
                        return Ok(DereferencedValue {
                            type_info,
                            state: VariableState::Unavailable(reason),
                        });
                    }
                    VariableState::Malformed(reason) => {
                        return Ok(DereferencedValue {
                            type_info,
                            state: VariableState::Malformed(reason),
                        });
                    }
                };
                let size = usize::try_from(shape.byte_size())
                    .map_err(|_| Error::debug_info(DwarfError::InvalidRange))?;
                let bytes = match implicit_pointer_bytes(&raw, byte_offset, size) {
                    Ok(bytes) => bytes,
                    Err(reason) => {
                        return Ok(DereferencedValue {
                            type_info,
                            state: VariableState::Unavailable(reason),
                        });
                    }
                };
                (VariableValueSource::Computed, bytes)
            }
        };
        let mut state = decode_value_state(&shape, context, raw.0, raw.1, self.target);
        self.constrain_dereference(&mut state, &shape);
        Ok(DereferencedValue { type_info, state })
    }
}

fn implicit_pointer_bytes(
    raw: &[u8],
    byte_offset: i64,
    size: usize,
) -> std::result::Result<Arc<[u8]>, VariableUnavailableReason> {
    let start = usize::try_from(byte_offset).map_err(|_| {
        VariableUnavailableReason::Other(
            "negative implicit-pointer offsets outside the referenced object are unsupported"
                .into(),
        )
    })?;
    let end = start
        .checked_add(size)
        .ok_or(VariableUnavailableReason::EvaluationLimit)?;
    let bytes = raw.get(start..end).ok_or_else(|| {
        VariableUnavailableReason::Other(
            "implicit-pointer offset is outside the referenced value".into(),
        )
    })?;
    Ok(Arc::from(bytes))
}

fn available_implicit_pointer(
    variable: &CatalogDataObject,
    type_info: TypeInfo,
    shape: &ValueShape,
    context: VariableContext,
    location: ImplicitPointerLocation,
) -> Variable {
    let state = match shape {
        ValueShape::Indirection {
            target,
            byte_size,
            address_class,
        } if location
            .size_in_bits
            .is_none_or(|bits| bits == byte_size.saturating_mul(8))
            && location.bit_offset.is_none() =>
        {
            let dereference = if *address_class != 0 {
                DereferenceState::Unavailable(DereferenceUnavailableReason::AddressClass(
                    *address_class,
                ))
            } else if let Some(target_type) = target {
                DereferenceState::Available(DereferenceReference {
                    stop_id: context.stop_id,
                    thread: context.thread,
                    module: context.module,
                    image: context.image,
                    context_address: context.address,
                    target_type: *target_type,
                    target: crate::model::DereferenceTarget::ImplicitPointer {
                        debug_info_offset: location.debug_info_offset,
                        byte_offset: location.byte_offset,
                    },
                })
            } else {
                DereferenceState::Unavailable(DereferenceUnavailableReason::UnspecifiedPointee)
            };
            VariableState::Available {
                source: VariableValueSource::ImplicitPointer,
                raw: None,
                value: VariableValue::ImplicitPointer,
                dereference,
            }
        }
        ValueShape::Indirection { .. } => {
            VariableState::Unavailable(crate::UnsupportedVariableFeature::CompositeLocation.into())
        }
        ValueShape::Scalar(_) => VariableState::Malformed(VariableMalformedReason {
            description: "DW_OP_implicit_pointer described a non-pointer value".into(),
        }),
    };
    Variable {
        kind: variable.kind,
        global: None,
        name: Arc::clone(&variable.name),
        declaration: variable.declaration.clone(),
        type_info: Some(type_info),
        state,
    }
}

impl ValueShape {
    const fn byte_size(&self) -> u64 {
        match self {
            Self::Scalar(base) => base.byte_size,
            Self::Indirection { byte_size, .. } => *byte_size,
        }
    }

    const fn scalar(&self) -> Option<&BaseType> {
        match self {
            Self::Scalar(base) => Some(base),
            Self::Indirection { .. } => None,
        }
    }
}

fn available(
    variable: &CatalogDataObject,
    type_info: TypeInfo,
    shape: &ValueShape,
    context: VariableContext,
    source: VariableValueSource,
    raw: Arc<[u8]>,
    target: TargetDescription,
) -> Variable {
    let state = decode_value_state(shape, context, source, raw, target);
    Variable {
        kind: variable.kind,
        global: None,
        name: Arc::clone(&variable.name),
        declaration: variable.declaration.clone(),
        type_info: Some(type_info),
        state,
    }
}

fn decode_value_state(
    shape: &ValueShape,
    context: VariableContext,
    source: VariableValueSource,
    raw: Arc<[u8]>,
    target: TargetDescription,
) -> VariableState {
    match shape {
        ValueShape::Scalar(base) => match decode_scalar(base, &raw, target) {
            Ok(value) => VariableState::Available {
                source,
                raw: Some(raw),
                value: VariableValue::Scalar(value),
                dereference: DereferenceState::NotApplicable,
            },
            Err(reason) => VariableState::Unavailable(reason),
        },
        ValueShape::Indirection {
            target: target_type,
            byte_size,
            address_class,
        } => match decode_address(&raw, *byte_size, target) {
            Ok(address) => {
                let dereference = if *address_class != 0 {
                    DereferenceState::Unavailable(DereferenceUnavailableReason::AddressClass(
                        *address_class,
                    ))
                } else if address.get() == 0 {
                    DereferenceState::Unavailable(DereferenceUnavailableReason::Null)
                } else if let Some(target_type) = target_type {
                    DereferenceState::Available(DereferenceReference {
                        stop_id: context.stop_id,
                        thread: context.thread,
                        module: context.module,
                        image: context.image,
                        context_address: context.address,
                        target_type: *target_type,
                        target: crate::model::DereferenceTarget::Address(address),
                    })
                } else {
                    DereferenceState::Unavailable(DereferenceUnavailableReason::UnspecifiedPointee)
                };
                VariableState::Available {
                    source,
                    raw: Some(raw),
                    value: VariableValue::Address(AddressValue { address }),
                    dereference,
                }
            }
            Err(reason) => VariableState::Unavailable(reason),
        },
    }
}

fn unavailable(
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

fn malformed(
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
        state: VariableState::Malformed(VariableMalformedReason { description }),
    }
}

fn resolve_frame_base(
    context: &mut FrameBaseContext<'_>,
    endian: RunTimeEndian,
    units: &[EvaluationUnit],
    runtime: &mut dyn VariableRuntime,
    budget: &mut EvaluationBudget,
) -> std::result::Result<VirtualAddress, EvaluateError> {
    if matches!(context.cache, FrameBaseCache::Empty) {
        *context.cache = match context.location {
            Metadata::Value(frame_base) => match frame_base.expression(context.address) {
                Ok(Some(expression)) => {
                    match evaluate_frame_base(expression, endian, units, runtime, budget) {
                        Ok(value) => FrameBaseCache::Available(value),
                        // The budget belongs to the current variable; a limit
                        // hit here must not poison the cache other variables
                        // share at this stop.
                        Err(VariableUnavailableReason::EvaluationLimit) => {
                            return Err(VariableUnavailableReason::EvaluationLimit.into());
                        }
                        Err(reason) => FrameBaseCache::Unavailable(reason),
                    }
                }
                Err(reason) => FrameBaseCache::Unavailable(reason),
                Ok(None) => {
                    FrameBaseCache::Unavailable("no frame base at the current instruction".into())
                }
            },
            Metadata::Unavailable(description) => {
                FrameBaseCache::Unavailable(Arc::clone(description).into())
            }
            Metadata::Malformed(description) => FrameBaseCache::Malformed(Arc::clone(description)),
        };
    }
    match context.cache {
        FrameBaseCache::Available(value) => Ok(*value),
        FrameBaseCache::Unavailable(reason) => Err(EvaluateError::Unavailable(reason.clone())),
        FrameBaseCache::Malformed(description) => {
            Err(EvaluateError::Malformed(Arc::clone(description)))
        }
        FrameBaseCache::Empty => unreachable!("frame base cache was populated"),
    }
}

fn evaluate_frame_base(
    expression: &Expression,
    endian: RunTimeEndian,
    units: &[EvaluationUnit],
    runtime: &mut dyn VariableRuntime,
    budget: &mut EvaluationBudget,
) -> std::result::Result<VirtualAddress, VariableUnavailableReason> {
    // A frame-base expression may not itself require a frame base.
    let pieces = match evaluate(
        expression,
        endian,
        &mut FrameBase::Unsupported,
        units,
        runtime,
        budget,
    ) {
        Ok(pieces) => pieces,
        Err(EvaluateError::Unavailable(reason)) => return Err(reason),
        Err(EvaluateError::Malformed(description)) => {
            return Err(VariableUnavailableReason::Other(description));
        }
    };
    let [piece] = pieces.as_slice() else {
        return Err("frame base is not one complete piece".into());
    };
    if piece.size_in_bits.is_some() || piece.bit_offset.is_some() {
        return Err("frame base is a partial piece".into());
    }
    match piece.location {
        Location::Address { address } => Ok(VirtualAddress::new(address)),
        Location::Register { register } => {
            register_u64(runtime, register.0, endian).map(VirtualAddress::new)
        }
        _ => Err("frame base did not evaluate to an address or register".into()),
    }
}

fn evaluate<'expression>(
    expression: &'expression Expression,
    endian: RunTimeEndian,
    frame_base: &mut FrameBase<'_>,
    units: &[EvaluationUnit],
    runtime: &mut dyn VariableRuntime,
    budget: &mut EvaluationBudget,
) -> std::result::Result<Vec<gimli::Piece<Reader<'expression>>>, EvaluateError> {
    let reader = gimli::EndianSlice::new(&expression.bytes, endian);
    let mut evaluation = gimli::Expression(reader).evaluation(expression.encoding);
    // Bound evaluation so a malformed expression with a backward branch cannot
    // hang the controller thread.
    evaluation.set_max_iterations(MAX_EVALUATION_ITERATIONS);
    let mut result = evaluation.evaluate().map_err(evaluation_error)?;
    loop {
        result = match result {
            EvaluationResult::Complete => return Ok(evaluation.result()),
            EvaluationResult::RequiresRegister {
                register,
                base_type,
            } => {
                let register = runtime.register(register.0)?;
                let value = evaluation_value(
                    &register.bytes,
                    evaluation_value_type(expression, units, base_type.0)?,
                    endian,
                )?;
                evaluation
                    .resume_with_register(value)
                    .map_err(evaluation_error)?
            }
            EvaluationResult::RequiresFrameBase => {
                let value = match frame_base {
                    FrameBase::Unsupported => {
                        return Err("frame base is unavailable".into());
                    }
                    FrameBase::Lazy(context) => {
                        resolve_frame_base(context, endian, units, runtime, budget)?
                    }
                };
                evaluation
                    .resume_with_frame_base(value.get())
                    .map_err(evaluation_error)?
            }
            EvaluationResult::RequiresCallFrameCfa => evaluation
                .resume_with_call_frame_cfa(runtime.call_frame_cfa()?.get())
                .map_err(evaluation_error)?,
            EvaluationResult::RequiresRelocatedAddress(address) => evaluation
                .resume_with_relocated_address(runtime.relocate(ImageAddress::new(address))?.get())
                .map_err(evaluation_error)?,
            EvaluationResult::RequiresIndexedAddress { index, relocate } => {
                let address = expression
                    .indexed_addresses
                    .get(&index.0)
                    .copied()
                    .ok_or_else(|| Arc::<str>::from("DWARF address index is unavailable"))?;
                let address = if relocate {
                    runtime.relocate(ImageAddress::new(address))?.get()
                } else {
                    address
                };
                evaluation
                    .resume_with_indexed_address(address)
                    .map_err(evaluation_error)?
            }
            EvaluationResult::RequiresBaseType(offset) => evaluation
                .resume_with_base_type(evaluation_value_type(expression, units, offset.0)?)
                .map_err(evaluation_error)?,
            EvaluationResult::RequiresMemory {
                address,
                size,
                space: None,
                base_type,
            } => {
                budget.consume_memory(usize::from(size))?;
                let bytes = runtime.read_memory(VirtualAddress::new(address), usize::from(size))?;
                let value = evaluation_value(
                    &bytes,
                    evaluation_value_type(expression, units, base_type.0)?,
                    endian,
                )?;
                evaluation
                    .resume_with_memory(value)
                    .map_err(evaluation_error)?
            }
            EvaluationResult::RequiresMemory { space: Some(_), .. } => {
                return Err(crate::UnsupportedVariableFeature::AddressSpace.into());
            }
            EvaluationResult::RequiresEntryValue(_) => {
                return Err(crate::UnsupportedVariableFeature::EntryValue.into());
            }
            EvaluationResult::RequiresParameterRef(_) => {
                return Err(crate::UnsupportedVariableFeature::ParameterReference.into());
            }
            EvaluationResult::RequiresAtLocation(_) => {
                return Err(crate::UnsupportedVariableFeature::CrossDieEvaluation.into());
            }
            EvaluationResult::RequiresTls(offset) => evaluation
                .resume_with_tls(runtime.tls_address(offset)?.get())
                .map_err(evaluation_error)?,
            EvaluationResult::RequiresWasmLocal { .. }
            | EvaluationResult::RequiresWasmGlobal { .. }
            | EvaluationResult::RequiresWasmStack { .. } => {
                return Err(crate::UnsupportedVariableFeature::WasmLocation.into());
            }
        };
    }
}

fn evaluation_value_type(
    expression: &Expression,
    units: &[EvaluationUnit],
    offset: usize,
) -> std::result::Result<gimli::ValueType, VariableUnavailableReason> {
    if offset == 0 {
        return Ok(gimli::ValueType::Generic);
    }
    units
        .get(expression.unit)
        .and_then(|unit| unit.base_types.get(&offset))
        .copied()
        .ok_or_else(|| crate::UnsupportedVariableFeature::TypedValue.into())
}

fn evaluation_value(
    bytes: &[u8],
    value_type: gimli::ValueType,
    endian: RunTimeEndian,
) -> std::result::Result<Value, VariableUnavailableReason> {
    if value_type == gimli::ValueType::Generic {
        return bytes_to_u64(bytes, endian).map(Value::Generic);
    }
    let size = usize::try_from(value_type.bit_size(u64::MAX) / 8).expect("value size fits usize");
    if bytes.len() < size {
        return Err("register or memory value is shorter than its DWARF type".into());
    }
    let bytes = match endian {
        RunTimeEndian::Little => &bytes[..size],
        RunTimeEndian::Big => &bytes[bytes.len() - size..],
    };
    Value::parse(value_type, gimli::EndianSlice::new(bytes, endian)).map_err(evaluation_error)
}

fn materialize_pieces(
    pieces: &[gimli::Piece<Reader<'_>>],
    byte_size: u64,
    scalar_type: Option<&BaseType>,
    endian: RunTimeEndian,
    target: TargetDescription,
    runtime: &mut dyn VariableRuntime,
    budget: &mut EvaluationBudget,
) -> std::result::Result<(VariableValueSource, Arc<[u8]>), VariableUnavailableReason> {
    if pieces.len() > MAX_LOCATION_PIECES {
        return Err(VariableUnavailableReason::EvaluationLimit);
    }
    let [piece] = pieces else {
        return Err(crate::UnsupportedVariableFeature::CompositeLocation.into());
    };
    let expected_bits = byte_size
        .checked_mul(8)
        .ok_or_else(|| Arc::<str>::from("scalar bit size overflow"))?;
    if piece.size_in_bits.is_some_and(|size| size != expected_bits) || piece.bit_offset.is_some() {
        return Err(crate::UnsupportedVariableFeature::CompositeLocation.into());
    }
    let size = usize::try_from(byte_size).expect("supported value size fits usize");
    match piece.location {
        Location::Empty => Err(VariableUnavailableReason::OptimizedOut),
        Location::Address { address } => {
            budget.consume_memory(size)?;
            runtime
                .read_memory(VirtualAddress::new(address), size)
                .map(|raw| {
                    (
                        VariableValueSource::Memory(VirtualAddress::new(address)),
                        raw,
                    )
                })
                .map_err(VariableUnavailableReason::Other)
        }
        Location::Register { register } => {
            let register = runtime.register(register.0)?;
            let raw = object_bytes(&register.bytes, size, endian)?;
            Ok((VariableValueSource::Register(register.descriptor), raw))
        }
        Location::Value { value } => Ok((
            VariableValueSource::Computed,
            if let Some(type_info) = scalar_type {
                dwarf_value_bytes(value, type_info, target)?
            } else {
                dwarf_address_bytes(value, size, target)?
            },
        )),
        Location::Bytes { ref value } => {
            let bytes = value.to_slice().map_err(evaluation_error)?.into_owned();
            if bytes.len() != size {
                return Err("implicit value size does not match its scalar type".into());
            }
            Ok((VariableValueSource::Constant, bytes.into()))
        }
        Location::ImplicitPointer { .. } => {
            Err(crate::UnsupportedVariableFeature::ImplicitPointer.into())
        }
    }
}

fn object_bytes(
    bytes: &[u8],
    size: usize,
    endian: RunTimeEndian,
) -> std::result::Result<Arc<[u8]>, VariableUnavailableReason> {
    if bytes.len() < size {
        return Err("register value is shorter than the scalar type".into());
    }
    Ok(match endian {
        RunTimeEndian::Little => Arc::from(&bytes[..size]),
        RunTimeEndian::Big => Arc::from(&bytes[bytes.len() - size..]),
    })
}

fn dwarf_value_bytes(
    value: Value,
    type_info: &BaseType,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, VariableUnavailableReason> {
    let size = usize::try_from(type_info.byte_size).expect("scalar size fits usize");
    let integer = match value {
        Value::Generic(value) | Value::U64(value) => Some(u128::from(value)),
        Value::U8(value) => Some(u128::from(value)),
        Value::U16(value) => Some(u128::from(value)),
        Value::U32(value) => Some(u128::from(value)),
        Value::I8(value) => Some(i128::from(value).cast_unsigned()),
        Value::I16(value) => Some(i128::from(value).cast_unsigned()),
        Value::I32(value) => Some(i128::from(value).cast_unsigned()),
        Value::I64(value) => Some(i128::from(value).cast_unsigned()),
        Value::F32(value) if size == 4 => {
            return integer_bytes(u128::from(value.to_bits()), size, target);
        }
        Value::F64(value) if size == 8 => {
            return integer_bytes(u128::from(value.to_bits()), size, target);
        }
        Value::F32(_) | Value::F64(_) => {
            return Err("computed floating-point size mismatch".into());
        }
    };
    let mut integer = integer.expect("integer DWARF values were classified above");
    // GCC and Clang represent optimized source booleans with word-sized
    // bitwise expressions (notably DW_OP_not). The source truth value is the
    // low bit after conversion to the declared one-byte boolean type.
    if type_info.encoding == BaseTypeEncoding::Boolean {
        integer &= 1;
    }
    wrapping_integer_bytes(integer, size, target)
}

fn wrapping_integer_bytes(
    value: u128,
    size: usize,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, VariableUnavailableReason> {
    if size == 0 || size > 16 {
        return Err("unsupported computed integer size".into());
    }
    let value = value & low_bits_mask(size * 8);
    let bytes = match target.byte_order {
        ByteOrder::Little => value.to_le_bytes()[..size].to_vec(),
        ByteOrder::Big => value.to_be_bytes()[16 - size..].to_vec(),
    };
    Ok(bytes.into())
}

fn materialize_constant(
    value: &ConstantValue,
    size: usize,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, VariableUnavailableReason> {
    match value {
        // Producers use DW_FORM_sdata when the implicit high bits are signed.
        // A fixed data form supplies zero high bits; the target type then
        // interprets the materialized byte pattern.
        ConstantValue::Unsigned(value) | ConstantValue::Fixed(value) => {
            integer_bytes(*value, size, target)
        }
        ConstantValue::Signed(value) => signed_integer_bytes(*value, size, target),
        ConstantValue::Bytes(bytes) if bytes.len() == size => Ok(Arc::clone(bytes)),
        ConstantValue::Bytes(_) => Err("constant value size does not match its scalar type".into()),
    }
}

fn dwarf_address_bytes(
    value: Value,
    size: usize,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, VariableUnavailableReason> {
    let address = match value {
        Value::Generic(value) | Value::U64(value) => value,
        Value::U8(value) => u64::from(value),
        Value::U16(value) => u64::from(value),
        Value::U32(value) => u64::from(value),
        Value::I8(value) => i64::from(value).cast_unsigned(),
        Value::I16(value) => i64::from(value).cast_unsigned(),
        Value::I32(value) => i64::from(value).cast_unsigned(),
        Value::I64(value) => value.cast_unsigned(),
        Value::F32(_) | Value::F64(_) => {
            return Err("floating-point value cannot represent an address".into());
        }
    };
    wrapping_integer_bytes(u128::from(address), size, target)
}

fn decode_address(
    raw: &[u8],
    byte_size: u64,
    target: TargetDescription,
) -> std::result::Result<VirtualAddress, VariableUnavailableReason> {
    let size = usize::try_from(byte_size).map_err(|_| {
        VariableUnavailableReason::Other("pointer size does not fit host usize".into())
    })?;
    if size == 0 || size > 8 || raw.len() != size {
        return Err("pointer representation is not a supported virtual address size".into());
    }
    let mut bytes = [0_u8; 8];
    match target.byte_order {
        ByteOrder::Little => bytes[..size].copy_from_slice(raw),
        ByteOrder::Big => bytes[8 - size..].copy_from_slice(raw),
    }
    let value = match target.byte_order {
        ByteOrder::Little => u64::from_le_bytes(bytes),
        ByteOrder::Big => u64::from_be_bytes(bytes),
    };
    Ok(VirtualAddress::new(value))
}

fn integer_bytes(
    value: u128,
    size: usize,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, VariableUnavailableReason> {
    if size == 0 || size > 16 || (size < 16 && value >= (1_u128 << (size * 8))) {
        return Err("constant value does not fit its scalar type".into());
    }
    let bytes = match target.byte_order {
        ByteOrder::Little => value.to_le_bytes()[..size].to_vec(),
        ByteOrder::Big => value.to_be_bytes()[16 - size..].to_vec(),
    };
    Ok(bytes.into())
}

fn signed_integer_bytes(
    value: i128,
    size: usize,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, VariableUnavailableReason> {
    if size == 0 || size > 16 {
        return Err("unsupported signed constant size".into());
    }
    let bits = size * 8;
    if bits < 128 {
        let minimum = -(1_i128 << (bits - 1));
        let maximum = (1_i128 << (bits - 1)) - 1;
        if !(minimum..=maximum).contains(&value) {
            return Err("signed constant value does not fit its scalar type".into());
        }
    }
    integer_bytes(value.cast_unsigned() & low_bits_mask(bits), size, target)
}

const fn low_bits_mask(bits: usize) -> u128 {
    if bits == 128 {
        u128::MAX
    } else {
        (1_u128 << bits) - 1
    }
}

fn register_u64(
    runtime: &mut dyn VariableRuntime,
    register: u16,
    endian: RunTimeEndian,
) -> std::result::Result<u64, VariableUnavailableReason> {
    let value = runtime.register(register)?;
    bytes_to_u64(&value.bytes, endian)
}

fn evaluation_error(error: gimli::Error) -> VariableUnavailableReason {
    VariableUnavailableReason::Other(format!("DWARF expression evaluation failed: {error}").into())
}

fn bytes_to_u64(
    bytes: &[u8],
    endian: RunTimeEndian,
) -> std::result::Result<u64, VariableUnavailableReason> {
    if bytes.len() > 8 {
        return Err("DWARF expression requested more than one word".into());
    }
    let mut word = [0_u8; 8];
    match endian {
        RunTimeEndian::Little => word[..bytes.len()].copy_from_slice(bytes),
        RunTimeEndian::Big => word[8 - bytes.len()..].copy_from_slice(bytes),
    }
    Ok(match endian {
        RunTimeEndian::Little => u64::from_le_bytes(word),
        RunTimeEndian::Big => u64::from_be_bytes(word),
    })
}

fn decode_scalar(
    type_info: &BaseType,
    bytes: &[u8],
    target: TargetDescription,
) -> std::result::Result<ScalarValue, VariableUnavailableReason> {
    let expected = usize::try_from(type_info.byte_size).expect("scalar byte size fits usize");
    if bytes.len() != expected {
        return Err("scalar storage size mismatch".into());
    }
    match type_info.encoding {
        BaseTypeEncoding::Boolean => match unsigned_value(bytes, target.byte_order)? {
            0 => Ok(ScalarValue::Boolean(false)),
            1 => Ok(ScalarValue::Boolean(true)),
            value => Err(format!("invalid boolean representation {value}").into()),
        },
        BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter => {
            Ok(ScalarValue::Signed(signed_value(bytes, target.byte_order)?))
        }
        BaseTypeEncoding::Unsigned | BaseTypeEncoding::UnsignedCharacter => Ok(
            ScalarValue::Unsigned(unsigned_value(bytes, target.byte_order)?),
        ),
        BaseTypeEncoding::Floating => decode_float(bytes, target).map(ScalarValue::Floating),
    }
}

fn unsigned_value(
    bytes: &[u8],
    byte_order: ByteOrder,
) -> std::result::Result<u128, VariableUnavailableReason> {
    if bytes.is_empty() || bytes.len() > 16 {
        return Err("unsupported integer storage size".into());
    }
    let mut value = [0_u8; 16];
    match byte_order {
        ByteOrder::Little => value[..bytes.len()].copy_from_slice(bytes),
        ByteOrder::Big => value[16 - bytes.len()..].copy_from_slice(bytes),
    }
    Ok(match byte_order {
        ByteOrder::Little => u128::from_le_bytes(value),
        ByteOrder::Big => u128::from_be_bytes(value),
    })
}

fn signed_value(
    bytes: &[u8],
    byte_order: ByteOrder,
) -> std::result::Result<i128, VariableUnavailableReason> {
    let unsigned = unsigned_value(bytes, byte_order)?;
    let bits = u32::try_from(bytes.len() * 8).expect("scalar bit count fits u32");
    if bits == 128 {
        return Ok(unsigned.cast_signed());
    }
    let shift = 128 - bits;
    Ok((unsigned << shift).cast_signed() >> shift)
}

fn decode_float(
    bytes: &[u8],
    target: TargetDescription,
) -> std::result::Result<FloatValue, VariableUnavailableReason> {
    match bytes.len() {
        4 => Ok(FloatValue::Binary32(
            u32::try_from(unsigned_value(bytes, target.byte_order)?).expect("four bytes fit u32"),
        )),
        8 => Ok(FloatValue::Binary64(
            u64::try_from(unsigned_value(bytes, target.byte_order)?).expect("eight bytes fit u64"),
        )),
        16 if target.architecture == Architecture::X86_64
            && target.byte_order == ByteOrder::Little =>
        {
            Ok(FloatValue::X87Extended {
                significand: u64::from_le_bytes(bytes[..8].try_into().expect("eight-byte slice")),
                sign_exponent: u16::from_le_bytes(bytes[8..10].try_into().expect("two-byte slice")),
            })
        }
        size => Err(format!("unsupported floating-point storage size {size}").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn specification_chains_reject_cycles() {
        let first = DieKey {
            unit: 0,
            offset: 0x10,
        };
        let second = DieKey {
            unit: 0,
            offset: 0x20,
        };
        let references = HashMap::from([(first, second), (second, first)]);

        assert!(matches!(
            checked_reference_chain(Some(first), |key| Ok(references.get(&key).copied())),
            Err(DwarfError::ReferenceCycle)
        ));
    }

    #[test]
    fn definition_index_collapses_aliases_but_never_conflicting_identities() {
        let identities = |die: &str, linkage: &str| [Arc::from(die), Arc::from(linkage)];
        let mut definitions = DefinitionIndex::default();

        assert_eq!(
            definitions.resolve(&identities("die:one", "linkage:same"), 0),
            DefinitionResolution::New
        );
        assert_eq!(
            definitions.resolve(&identities("die:two", "linkage:same"), 1),
            DefinitionResolution::Existing(0)
        );
        assert_eq!(
            definitions.resolve(&identities("die:three", "linkage:other"), 1),
            DefinitionResolution::New
        );
        assert_eq!(
            definitions.resolve(&identities("die:three", "linkage:same"), 2),
            DefinitionResolution::Conflict
        );
    }

    struct Runtime {
        registers: BTreeMap<u16, u64>,
        cfa: std::result::Result<VirtualAddress, VariableUnavailableReason>,
        memory: Option<Arc<[u8]>>,
        memory_reads: u32,
    }

    impl VariableRuntime for Runtime {
        fn register(
            &mut self,
            register: u16,
        ) -> std::result::Result<crate::debug_info::VariableRegister, VariableUnavailableReason>
        {
            let value = self.registers.get(&register).copied().ok_or_else(|| {
                VariableUnavailableReason::RegisterUnavailable(register.to_string().into())
            })?;
            Ok(crate::debug_info::VariableRegister {
                descriptor: crate::RegisterDescriptor {
                    id: crate::RegisterId::new(u32::from(register)),
                    name: format!("r{register}").into(),
                    bits: 64,
                    role: None,
                },
                bytes: Arc::from(value.to_le_bytes()),
            })
        }

        fn call_frame_cfa(&self) -> std::result::Result<VirtualAddress, VariableUnavailableReason> {
            self.cfa.clone()
        }

        fn tls_address(
            &mut self,
            _offset: u64,
        ) -> std::result::Result<VirtualAddress, VariableUnavailableReason> {
            Err(crate::UnsupportedVariableFeature::Tls.into())
        }

        fn relocate(&self, address: ImageAddress) -> std::result::Result<VirtualAddress, Arc<str>> {
            Ok(VirtualAddress::new(address.get()))
        }

        fn read_memory(
            &mut self,
            _address: VirtualAddress,
            size: usize,
        ) -> std::result::Result<Arc<[u8]>, Arc<str>> {
            self.memory_reads += 1;
            self.memory
                .as_ref()
                .map(|memory| Arc::from(&memory[..size]))
                .ok_or_else(|| Arc::from("unexpected memory read"))
        }
    }

    fn expression(bytes: &[u8]) -> Expression {
        Expression {
            bytes: Arc::from(bytes),
            encoding: gimli::Encoding {
                format: gimli::Format::Dwarf32,
                version: 5,
                address_size: 8,
            },
            unit: 0,
            indexed_addresses: Arc::new(HashMap::new()),
        }
    }

    fn scalar_type(encoding: BaseTypeEncoding, byte_size: u64) -> BaseType {
        BaseType {
            name: "test".into(),
            base_name: "test".into(),
            encoding,
            byte_size,
        }
    }

    fn target(byte_order: ByteOrder) -> TargetDescription {
        TargetDescription {
            architecture: Architecture::X86_64,
            byte_order,
            pointer_width: crate::PointerWidth::Bits64,
        }
    }

    fn units(
        base_types: impl IntoIterator<Item = (usize, gimli::ValueType)>,
    ) -> Vec<EvaluationUnit> {
        vec![EvaluationUnit {
            base_types: base_types.into_iter().collect(),
        }]
    }

    #[test]
    fn frame_base_register_and_fbreg_location_have_distinct_meanings() {
        let mut runtime = Runtime {
            registers: BTreeMap::from([(6, 0x2000)]),
            cfa: Ok(VirtualAddress::new(0x3000)),
            memory: None,
            memory_reads: 0,
        };
        let frame_base = evaluate_frame_base(
            &expression(&[gimli::DW_OP_reg6.0]),
            RunTimeEndian::Little,
            &units([]),
            &mut runtime,
            &mut EvaluationBudget::default(),
        )
        .expect("register-valued frame base");
        assert_eq!(frame_base, VirtualAddress::new(0x2000));

        let fbreg = expression(&[gimli::DW_OP_fbreg.0, 0x70]);
        let location = Metadata::Value(LocationDescription {
            entries: vec![LocationEntry {
                range: None,
                expression: expression(&[gimli::DW_OP_reg6.0]),
            }]
            .into(),
        });
        let mut cache = FrameBaseCache::Empty;
        let pieces = evaluate(
            &fbreg,
            RunTimeEndian::Little,
            &mut FrameBase::Lazy(FrameBaseContext {
                location: &location,
                address: Some(ImageAddress::new(0)),
                cache: &mut cache,
            }),
            &units([]),
            &mut runtime,
            &mut EvaluationBudget::default(),
        )
        .expect("frame-relative memory location");
        assert!(matches!(cache, FrameBaseCache::Available(_)));
        assert!(matches!(
            pieces.as_slice(),
            [gimli::Piece {
                location: Location::Address { address: 0x1ff0 },
                ..
            }]
        ));

        let direct_register = expression(&[gimli::DW_OP_reg6.0]);
        let pieces = evaluate(
            &direct_register,
            RunTimeEndian::Little,
            &mut FrameBase::Unsupported,
            &units([]),
            &mut runtime,
            &mut EvaluationBudget::default(),
        )
        .expect("direct register location");
        assert!(matches!(
            pieces.as_slice(),
            [gimli::Piece {
                location: Location::Register {
                    register: gimli::Register(6)
                },
                ..
            }]
        ));
    }

    #[test]
    fn cfa_expression_limit_remains_a_typed_unavailable_reason() {
        let mut runtime = Runtime {
            registers: BTreeMap::new(),
            cfa: Err(VariableUnavailableReason::CfaExpression),
            memory: None,
            memory_reads: 0,
        };
        assert_eq!(
            evaluate_frame_base(
                &expression(&[gimli::DW_OP_call_frame_cfa.0]),
                RunTimeEndian::Little,
                &units([]),
                &mut runtime,
                &mut EvaluationBudget::default(),
            ),
            Err(VariableUnavailableReason::CfaExpression)
        );
    }

    #[test]
    fn malformed_backward_branch_expression_fails_instead_of_hanging() {
        let mut runtime = Runtime {
            registers: BTreeMap::new(),
            cfa: Ok(VirtualAddress::new(0x3000)),
            memory: None,
            memory_reads: 0,
        };
        // DW_OP_skip with a -3 offset branches back onto itself forever.
        let looping = expression(&[gimli::DW_OP_skip.0, 0xfd, 0xff]);
        let result = evaluate(
            &looping,
            RunTimeEndian::Little,
            &mut FrameBase::Unsupported,
            &units([]),
            &mut runtime,
            &mut EvaluationBudget::default(),
        );
        assert!(result.is_err(), "infinite expression must be rejected");
    }

    #[test]
    fn typed_register_values_are_evaluated_with_the_referenced_base_type() {
        let mut runtime = Runtime {
            registers: BTreeMap::from([(6, 0x2000)]),
            cfa: Ok(VirtualAddress::new(0x3000)),
            memory: None,
            memory_reads: 0,
        };
        // DW_OP_regval_type register 6, base type DIE offset 0x10.
        let typed = expression(&[
            gimli::DW_OP_regval_type.0,
            6,
            0x10,
            gimli::DW_OP_stack_value.0,
        ]);
        let result = evaluate(
            &typed,
            RunTimeEndian::Little,
            &mut FrameBase::Unsupported,
            &units([(0x10, gimli::ValueType::U64)]),
            &mut runtime,
            &mut EvaluationBudget::default(),
        )
        .expect("typed register expression");
        assert!(matches!(
            result.as_slice(),
            [gimli::Piece {
                location: Location::Value {
                    value: Value::U64(0x2000)
                },
                ..
            }]
        ));
    }

    #[test]
    fn implicit_and_computed_values_materialize_with_source_provenance() {
        let mut runtime = Runtime {
            registers: BTreeMap::new(),
            cfa: Ok(VirtualAddress::new(0x3000)),
            memory: None,
            memory_reads: 0,
        };
        let implicit = expression(&[gimli::DW_OP_implicit_value.0, 4, 0xd6, 0xff, 0xff, 0xff]);
        let pieces = evaluate(
            &implicit,
            RunTimeEndian::Little,
            &mut FrameBase::Unsupported,
            &units([]),
            &mut runtime,
            &mut EvaluationBudget::default(),
        )
        .expect("implicit scalar expression");
        let materialized = materialize_pieces(
            &pieces,
            4,
            Some(&scalar_type(BaseTypeEncoding::Signed, 4)),
            RunTimeEndian::Little,
            target(ByteOrder::Little),
            &mut runtime,
            &mut EvaluationBudget::default(),
        )
        .expect("implicit scalar value");
        assert_eq!(materialized.0, VariableValueSource::Constant);
        assert_eq!(materialized.1.as_ref(), &[0xd6, 0xff, 0xff, 0xff]);

        let computed = dwarf_value_bytes(
            Value::Generic(u64::MAX - 1),
            &scalar_type(BaseTypeEncoding::Boolean, 1),
            target(ByteOrder::Little),
        )
        .expect("word-sized boolean expression");
        assert_eq!(computed.as_ref(), &[0]);
    }

    #[test]
    fn deferred_operations_and_missing_types_have_stable_typed_reasons() {
        let mut runtime = Runtime {
            registers: BTreeMap::from([(0, 1)]),
            cfa: Ok(VirtualAddress::new(0x3000)),
            memory: None,
            memory_reads: 0,
        };
        let entry = expression(&[
            gimli::DW_OP_entry_value.0,
            1,
            gimli::DW_OP_reg0.0,
            gimli::DW_OP_stack_value.0,
        ]);
        assert_eq!(
            evaluate(
                &entry,
                RunTimeEndian::Little,
                &mut FrameBase::Unsupported,
                &units([]),
                &mut runtime,
                &mut EvaluationBudget::default(),
            ),
            Err(crate::UnsupportedVariableFeature::EntryValue.into())
        );

        let missing_type = expression(&[
            gimli::DW_OP_regval_type.0,
            0,
            0x10,
            gimli::DW_OP_stack_value.0,
        ]);
        assert_eq!(
            evaluate(
                &missing_type,
                RunTimeEndian::Little,
                &mut FrameBase::Unsupported,
                &units([]),
                &mut runtime,
                &mut EvaluationBudget::default(),
            ),
            Err(crate::UnsupportedVariableFeature::TypedValue.into())
        );
    }

    #[test]
    fn expression_memory_reads_are_strictly_bounded() {
        let mut bytes = Vec::new();
        for address in 0..=MAX_EVALUATION_MEMORY_READS {
            bytes.push(gimli::DW_OP_addr.0);
            bytes.extend_from_slice(&u64::from(address).to_le_bytes());
            bytes.extend_from_slice(&[gimli::DW_OP_deref_size.0, 1, gimli::DW_OP_drop.0]);
        }
        bytes.extend_from_slice(&[gimli::DW_OP_lit0.0, gimli::DW_OP_stack_value.0]);
        let expression = expression(&bytes);
        let mut runtime = Runtime {
            registers: BTreeMap::new(),
            cfa: Ok(VirtualAddress::new(0x3000)),
            memory: Some(Arc::from([0_u8; 16])),
            memory_reads: 0,
        };
        assert_eq!(
            evaluate(
                &expression,
                RunTimeEndian::Little,
                &mut FrameBase::Unsupported,
                &units([]),
                &mut runtime,
                &mut EvaluationBudget::default(),
            ),
            Err(VariableUnavailableReason::EvaluationLimit.into())
        );
        assert_eq!(runtime.memory_reads, MAX_EVALUATION_MEMORY_READS);
    }

    #[test]
    fn fixed_width_constants_take_signedness_from_the_variable_type() {
        let little = target(ByteOrder::Little);
        // DW_FORM_data1 0xff for a signed 4-byte type is -1, not 255.
        let fixed = ConstantValue::Fixed(0xff);
        assert_eq!(
            materialize_constant(&fixed, 4, little)
                .expect("zero-extended fixed-form constant")
                .as_ref(),
            &[0xff, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            materialize_constant(&fixed, 4, little)
                .expect("zero-extended constant")
                .as_ref(),
            &[0xff, 0x00, 0x00, 0x00]
        );
        // A non-negative fixed-width value is unchanged by sign extension.
        let positive = ConstantValue::Fixed(0x7f);
        assert_eq!(
            materialize_constant(&positive, 2, little)
                .expect("positive constant")
                .as_ref(),
            &[0x7f, 0x00]
        );
    }

    #[test]
    fn unexecuted_fbreg_branches_do_not_require_a_frame_base() {
        let mut runtime = Runtime {
            registers: BTreeMap::new(),
            cfa: Ok(VirtualAddress::new(0x3000)),
            memory: None,
            memory_reads: 0,
        };
        // DW_OP_lit1 then DW_OP_bra +2 skips the DW_OP_fbreg on the executed
        // path; the frame base must not be resolved eagerly.
        let branching = expression(&[
            gimli::DW_OP_lit1.0,
            gimli::DW_OP_bra.0,
            0x02,
            0x00,
            gimli::DW_OP_fbreg.0,
            0x00,
            gimli::DW_OP_lit0.0,
            gimli::DW_OP_stack_value.0,
        ]);
        let location = Metadata::Unavailable("no frame base metadata".into());
        let mut cache = FrameBaseCache::Empty;
        let pieces = evaluate(
            &branching,
            RunTimeEndian::Little,
            &mut FrameBase::Lazy(FrameBaseContext {
                location: &location,
                address: Some(ImageAddress::new(0)),
                cache: &mut cache,
            }),
            &units([]),
            &mut runtime,
            &mut EvaluationBudget::default(),
        )
        .expect("executed path never needs the frame base");
        assert!(matches!(
            pieces.as_slice(),
            [gimli::Piece {
                location: Location::Value {
                    value: Value::Generic(0)
                },
                ..
            }]
        ));
        assert!(
            matches!(cache, FrameBaseCache::Empty),
            "frame base must not be resolved for an unexecuted branch"
        );

        // The same expression taking the fbreg path surfaces the metadata
        // failure lazily.
        let taken = expression(&[gimli::DW_OP_fbreg.0, 0x00, gimli::DW_OP_stack_value.0]);
        let result = evaluate(
            &taken,
            RunTimeEndian::Little,
            &mut FrameBase::Lazy(FrameBaseContext {
                location: &location,
                address: Some(ImageAddress::new(0)),
                cache: &mut cache,
            }),
            &units([]),
            &mut runtime,
            &mut EvaluationBudget::default(),
        );
        assert!(matches!(result, Err(EvaluateError::Unavailable(_))));
    }

    #[test]
    fn specific_location_entries_override_default_entries() {
        let range = |start: u64, end: u64| {
            Some(AddressRange {
                start: ImageAddress::new(start),
                end: ImageAddress::new(end),
            })
        };
        let description = LocationDescription {
            entries: vec![
                LocationEntry {
                    range: None,
                    expression: expression(&[gimli::DW_OP_reg0.0]),
                },
                LocationEntry {
                    range: range(0x100, 0x200),
                    expression: expression(&[gimli::DW_OP_reg1.0]),
                },
            ]
            .into(),
        };

        let specific = description
            .expression(Some(ImageAddress::new(0x150)))
            .expect("specific entry wins inside its range")
            .expect("an expression is active");
        assert_eq!(specific.bytes.as_ref(), &[gimli::DW_OP_reg1.0]);

        let fallback = description
            .expression(Some(ImageAddress::new(0x300)))
            .expect("default entry applies outside all ranges")
            .expect("an expression is active");
        assert_eq!(fallback.bytes.as_ref(), &[gimli::DW_OP_reg0.0]);

        // A range-less default still resolves without an instruction context.
        let without_context = description
            .expression(None)
            .expect("default entry applies without a context")
            .expect("an expression is active");
        assert_eq!(without_context.bytes.as_ref(), &[gimli::DW_OP_reg0.0]);

        // A location with only range-gated entries must refuse to guess when no
        // instruction context is available rather than silently resolving.
        let ranged_only = LocationDescription {
            entries: vec![LocationEntry {
                range: range(0x100, 0x200),
                expression: expression(&[gimli::DW_OP_reg1.0]),
            }]
            .into(),
        };
        assert!(ranged_only.expression(None).is_err());

        let overlapping = LocationDescription {
            entries: vec![
                LocationEntry {
                    range: range(0x100, 0x200),
                    expression: expression(&[gimli::DW_OP_reg0.0]),
                },
                LocationEntry {
                    range: range(0x180, 0x280),
                    expression: expression(&[gimli::DW_OP_reg1.0]),
                },
            ]
            .into(),
        };
        assert!(
            overlapping
                .expression(Some(ImageAddress::new(0x190)))
                .is_err()
        );
    }

    #[test]
    fn scalar_decoding_obeys_width_sign_and_target_byte_order() {
        for (bytes, byte_order, signed, unsigned) in [
            (&[0xfe][..], ByteOrder::Little, -2_i128, 254_u128),
            (&[0xfe, 0xff], ByteOrder::Little, -2, 65_534),
            (&[0xff, 0xfe], ByteOrder::Big, -2, 65_534),
            (
                &[0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
                ByteOrder::Little,
                -2,
                u128::from(u64::MAX - 1),
            ),
        ] {
            assert_eq!(
                decode_scalar(
                    &scalar_type(BaseTypeEncoding::Signed, bytes.len() as u64),
                    bytes,
                    target(byte_order),
                )
                .expect("signed scalar"),
                ScalarValue::Signed(signed)
            );
            assert_eq!(
                decode_scalar(
                    &scalar_type(BaseTypeEncoding::Unsigned, bytes.len() as u64),
                    bytes,
                    target(byte_order),
                )
                .expect("unsigned scalar"),
                ScalarValue::Unsigned(unsigned)
            );
        }
    }

    #[test]
    fn pointer_decoding_obeys_target_width_and_byte_order_without_truncation() {
        assert_eq!(
            decode_address(&[0x78, 0x56, 0x34, 0x12], 4, target(ByteOrder::Little),)
                .expect("little-endian 32-bit address"),
            VirtualAddress::new(0x1234_5678),
        );
        assert_eq!(
            decode_address(&[0x12, 0x34, 0x56, 0x78], 4, target(ByteOrder::Big),)
                .expect("big-endian 32-bit address"),
            VirtualAddress::new(0x1234_5678),
        );
        assert_eq!(
            decode_address(
                &0x0123_4567_89ab_cdef_u64.to_le_bytes(),
                8,
                target(ByteOrder::Little),
            )
            .expect("64-bit address"),
            VirtualAddress::new(0x0123_4567_89ab_cdef),
        );
        assert!(decode_address(&[0; 9], 9, target(ByteOrder::Little)).is_err());
        assert!(decode_address(&[0; 4], 8, target(ByteOrder::Little)).is_err());
    }

    #[test]
    fn implicit_pointer_offsets_are_bounded_and_never_return_partial_values() {
        assert_eq!(
            implicit_pointer_bytes(&[1, 2, 3, 4, 5, 6, 7, 8], 4, 4)
                .expect("in-bounds subobject")
                .as_ref(),
            &[5, 6, 7, 8]
        );
        assert!(matches!(
            implicit_pointer_bytes(&[0; 8], -1, 4),
            Err(VariableUnavailableReason::Other(_))
        ));
        assert!(matches!(
            implicit_pointer_bytes(&[0; 8], 6, 4),
            Err(VariableUnavailableReason::Other(_))
        ));
        assert_eq!(
            implicit_pointer_bytes(&[0; 8], i64::MAX, usize::MAX),
            Err(VariableUnavailableReason::EvaluationLimit)
        );
    }

    #[test]
    fn type_graph_rejects_wrapper_cycles_but_permits_recursive_pointer_edges() {
        let image = ModuleImageId::new(7);
        let reference = |id| TypeReference {
            image,
            id: TypeId::new(id),
        };
        let cycle = [
            TypeEntry::Resolved(TypeInfo {
                reference: reference(0),
                name: "left".into(),
                byte_size: Some(8),
                kind: TypeKind::Alias {
                    target: reference(1),
                },
            }),
            TypeEntry::Resolved(TypeInfo {
                reference: reference(1),
                name: "right".into(),
                byte_size: Some(8),
                kind: TypeKind::Qualified {
                    qualifier: TypeQualifier::Const,
                    target: reference(0),
                },
            }),
        ];
        assert_eq!(
            value_shape_from(&cycle, TypeId::new(0))
                .unwrap_err()
                .as_ref(),
            "type wrapper cycle",
        );

        let recursive_pointer = [TypeEntry::Resolved(TypeInfo {
            reference: reference(0),
            name: "node *".into(),
            byte_size: Some(8),
            kind: TypeKind::Pointer {
                target: Some(reference(0)),
                address_class: 0,
            },
        })];
        assert!(matches!(
            value_shape_from(&recursive_pointer, TypeId::new(0)),
            Ok(ValueShape::Indirection {
                target: Some(id),
                byte_size: 8,
                address_class: 0,
            }) if id == TypeId::new(0)
        ));
    }

    #[test]
    fn null_and_non_default_address_classes_never_create_read_capabilities() {
        let context = VariableContext {
            stop_id: crate::StopId::new(9),
            thread: crate::ThreadId::new(10),
            module: crate::ModuleId::new(11),
            image: ModuleImageId::new(12),
            address: None,
        };
        let shape = |address_class| ValueShape::Indirection {
            target: Some(TypeId::new(1)),
            byte_size: 8,
            address_class,
        };
        assert!(matches!(
            decode_value_state(
                &shape(0),
                context,
                VariableValueSource::Computed,
                Arc::from([0_u8; 8]),
                target(ByteOrder::Little),
            ),
            VariableState::Available {
                dereference: DereferenceState::Unavailable(DereferenceUnavailableReason::Null),
                ..
            }
        ));
        assert!(matches!(
            decode_value_state(
                &shape(17),
                context,
                VariableValueSource::Computed,
                Arc::from(1_u64.to_le_bytes()),
                target(ByteOrder::Little),
            ),
            VariableState::Available {
                dereference: DereferenceState::Unavailable(
                    DereferenceUnavailableReason::AddressClass(17)
                ),
                ..
            }
        ));
    }

    #[test]
    fn boolean_and_float_decoding_preserve_exact_representations() {
        let little = target(ByteOrder::Little);
        assert_eq!(
            decode_scalar(&scalar_type(BaseTypeEncoding::Boolean, 1), &[0], little,)
                .expect("false"),
            ScalarValue::Boolean(false)
        );
        assert!(decode_scalar(&scalar_type(BaseTypeEncoding::Boolean, 1), &[2], little,).is_err());
        assert_eq!(
            decode_scalar(
                &scalar_type(BaseTypeEncoding::Floating, 4),
                &1.25_f32.to_bits().to_le_bytes(),
                little,
            )
            .expect("binary32"),
            ScalarValue::Floating(FloatValue::Binary32(1.25_f32.to_bits()))
        );
        assert_eq!(
            decode_scalar(
                &scalar_type(BaseTypeEncoding::Floating, 8),
                &(-0.0_f64).to_bits().to_le_bytes(),
                little,
            )
            .expect("binary64"),
            ScalarValue::Floating(FloatValue::Binary64((-0.0_f64).to_bits()))
        );

        let mut extended = [0xa5_u8; 16];
        extended[..8].copy_from_slice(&0xc800_0000_0000_0000_u64.to_le_bytes());
        extended[8..10].copy_from_slice(&0x4000_u16.to_le_bytes());
        assert_eq!(
            decode_scalar(
                &scalar_type(BaseTypeEncoding::Floating, 16),
                &extended,
                little,
            )
            .expect("x87 extended"),
            ScalarValue::Floating(FloatValue::X87Extended {
                significand: 0xc800_0000_0000_0000,
                sign_exponent: 0x4000,
            })
        );
    }
}
