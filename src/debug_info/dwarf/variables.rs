use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use gimli::{EvaluationResult, Location, Reader as _, RunTimeEndian, Value};

use super::{
    DieKey, DwarfError, Reader, TypeSignatures, UnitCatalog, die_reference,
    die_reference_with_signatures, is_type_unit, source_file_id, source_path,
    type_unit_source_file_id,
};
use crate::debug_info::{VariableContext, VariableInfo, VariableRuntime};
use crate::model::ArrayDimension;
use crate::{
    Accessibility, ActiveVariantValue, AddressRange, AddressValue, Architecture, BaseClass,
    BaseClassValue, BaseClassVirtuality, BaseType, BaseTypeEncoding, ByteOrder, CodeInstanceId,
    ColumnNumber, DereferenceReference, DereferenceState, DereferenceUnavailableReason,
    DereferencedValue, EnumerationOrigin, Enumerator, Error, FloatValue, GlobalVariableId,
    GlobalVariableInfo, GlobalVariableType, GlobalVariableVisibility, ImageAddress, InspectedValue,
    InspectionLimit, IntegerValue, LineNumber, ModuleImageId, NamedTypeRelationship, RecordKind,
    RecordMember, RecordMemberLayout, RecordMemberValue, ReferenceKind, Result, ScalarValue,
    SourceFile, SourceFileId, SourceLocation, TargetDescription, TypeId, TypeInfo, TypeKind,
    TypeModifier, TypeNode, TypeReference, ValueGraph, ValueNode, ValueNodeId, ValueNodeState,
    Variable, VariableKind, VariableMalformedReason, VariableQuery, VariableState,
    VariableUnavailableReason, VariableValue, VariableValueSource, Variant, VariantDiscriminant,
    VariantSelection, VariantSelector, VariantStorageKind, VirtualAddress,
};

const MAX_SCALAR_BYTES: u64 = 16;
const MAX_EVALUATION_ITERATIONS: u32 = 10_000;
const MAX_EVALUATION_MEMORY_READS: u32 = 64;
const MAX_EVALUATION_MEMORY_BYTES: usize = 1_024;
const MAX_LOCATION_PIECES: usize = 64;
const MAX_VALUE_NODES: usize = 4_096;
const MAX_TYPES: usize = 65_536;
const MAX_TYPE_RESOLUTION_DEPTH: usize = 256;
const MAX_RECORD_CHILDREN: usize = 4_096;
const MAX_VARIANT_METADATA: usize = 4_096;
const MAX_SYMBOLIC_NAMES: usize = 262_144;
const MAX_AGGREGATE_DEPTH: usize = 64;

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

#[derive(Debug, Clone)]
enum TypeEntry {
    Building,
    Resolved(TypeInfo),
    Malformed(Arc<str>),
}

fn variant_metadata_limit_type(
    reference: TypeReference,
    name: &Arc<str>,
    byte_size: Option<u64>,
) -> TypeEntry {
    TypeEntry::Resolved(TypeInfo {
        reference,
        name: Arc::clone(name),
        byte_size,
        kind: TypeKind::Opaque {
            description: "variant metadata exceeds its resource limit".into(),
        },
    })
}

struct TypeArenaBuilder<'a, 'data> {
    dwarf: &'a gimli::Dwarf<Reader<'data>>,
    units: &'a [gimli::Unit<Reader<'data>>],
    type_signatures: &'a TypeSignatures,
    image: ModuleImageId,
    by_die: HashMap<DieKey, TypeId>,
    type_definitions: HashMap<DieKey, DieKey>,
    ambiguous_type_declarations: HashSet<DieKey>,
    entries: Vec<TypeEntry>,
    /// DIE-boundary offsets per unit, indexed by unit position. A `DW_AT_type`
    /// offset that is not in its unit's set points into the middle of a DIE and
    /// is defective. Built once so target validation stays O(1) per reference.
    die_offsets: Vec<HashSet<usize>>,
    unit_languages: Vec<Option<gimli::DwLang>>,
    zig_units: Vec<bool>,
    explicit_names: HashSet<TypeId>,
    resolution_depth: usize,
    byte_order: ByteOrder,
    limit_type: Option<TypeId>,
    dynamic_record_layouts: HashMap<DynamicAggregateLayoutKey, Expression>,
    record_member_declarations: Vec<AggregateMemberDeclaration>,
    symbolic_names: usize,
}

#[derive(Clone, Copy)]
struct AggregateMemberDeclaration {
    aggregate: TypeId,
    member: AggregateMemberPath,
    die: DieKey,
}

#[derive(Clone, Copy)]
enum AggregateMemberPath {
    Direct(usize),
    Discriminant,
    Variant { variant: usize, member: usize },
}

enum NamedConstantCollection {
    Enumerators(Vec<Enumerator>),
    Malformed(Arc<str>),
    Limit,
}

#[derive(Debug, PartialEq, Eq)]
enum VariantMetadataError {
    Malformed(Arc<str>),
    Limit,
}

impl From<Arc<str>> for VariantMetadataError {
    fn from(reason: Arc<str>) -> Self {
        Self::Malformed(reason)
    }
}

#[derive(Default)]
struct VariantMetadataBudget {
    items: usize,
}

impl VariantMetadataBudget {
    const fn consume(&mut self) -> std::result::Result<(), VariantMetadataError> {
        if self.items >= MAX_VARIANT_METADATA {
            return Err(VariantMetadataError::Limit);
        }
        self.items += 1;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct DynamicAggregateLayoutKey {
    aggregate: TypeId,
    child: DynamicAggregateChild,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum DynamicAggregateChild {
    Member(usize),
    Base(usize),
    Discriminant,
    VariantMember { variant: usize, member: usize },
}

#[derive(Clone, Debug)]
struct ValueShape {
    type_info: TypeInfo,
    kind: ValueShapeKind,
}

#[derive(Clone, Debug)]
enum ValueShapeKind {
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

#[derive(Clone)]
enum PathStep {
    Dereference {
        target: TypeId,
        byte_size: u64,
        address_class: u64,
    },
    Member(Box<PlannedMemberStep>),
    Unavailable(VariableUnavailableReason),
}

#[derive(Clone)]
struct PlannedMemberStep {
    aggregate: TypeId,
    child: DynamicAggregateChild,
    member: RecordMember,
    required_variant: Option<(usize, VariantDiscriminant, Arc<[Variant]>)>,
}

struct PlannedPath {
    steps: Vec<PathStep>,
    terminal: Option<TypeId>,
}

#[derive(Clone)]
enum LocatedStorage {
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

enum PathEvaluationError {
    Unavailable(VariableUnavailableReason),
    Malformed(Arc<str>),
}

fn static_member_layout_is_valid(
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

impl From<VariableUnavailableReason> for PathEvaluationError {
    fn from(reason: VariableUnavailableReason) -> Self {
        Self::Unavailable(reason)
    }
}

impl From<crate::UnsupportedVariableFeature> for PathEvaluationError {
    fn from(feature: crate::UnsupportedVariableFeature) -> Self {
        Self::Unavailable(feature.into())
    }
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
        if is_type_unit(unit) {
            continue;
        }
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
        if is_type_unit(unit) {
            continue;
        }
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
            let info = GlobalVariableInfo {
                id: GlobalVariableId::new(
                    u32::try_from(globals.len()).expect("global count fits u32"),
                ),
                name,
                qualified_name,
                linkage_name: linkage_name.clone(),
                declaration: declaration.ok().flatten(),
                // This copy is replaced after graph finalization. Keeping the
                // initial state accurate makes the builder invariant explicit
                // without publishing construction-only names or sizes.
                type_info: public_global_type(&type_info, &types.entries),
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

fn public_global_type(resolution: &TypeResolution, types: &[TypeEntry]) -> GlobalVariableType {
    match resolution {
        TypeResolution::Resolved(id) => {
            match types.get(usize::try_from(id.get()).expect("type ID fits usize")) {
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
            }
        }
        TypeResolution::Malformed(description) => {
            GlobalVariableType::Malformed(VariableMalformedReason {
                description: Arc::clone(description),
            })
        }
    }
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

fn strict_flag(
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    attribute: gimli::DwAt,
) -> std::result::Result<bool, Arc<str>> {
    match entry.attr_value(attribute) {
        None => Ok(false),
        Some(gimli::AttributeValue::Flag(value)) => Ok(value),
        Some(_) => Err(format!("{attribute:?} has an invalid flag encoding").into()),
    }
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
    let file = if is_type_unit(file_unit) {
        type_unit_source_file_id(path, source_files, source_file_ids)
    } else {
        source_file_id(path, source_files, source_file_ids)
    };
    Ok(Some(SourceLocation {
        file,
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

fn integer_bit_width(base: &BaseType) -> std::result::Result<u32, Arc<str>> {
    let storage_bits = base
        .byte_size
        .checked_mul(8)
        .ok_or_else(|| Arc::from("integer storage bit width overflows"))?;
    let bits = base.bit_size.unwrap_or(storage_bits);
    if bits == 0 || bits > storage_bits || bits > 128 {
        return Err("integer bit width is outside its storage representation".into());
    }
    u32::try_from(bits).map_err(|_| Arc::from("integer bit width exceeds u32"))
}

fn checked_integer_value(
    value: IntegerValue,
    base: &BaseType,
) -> std::result::Result<IntegerValue, Arc<str>> {
    let bits = integer_bit_width(base)?;
    match (base.encoding, value) {
        (
            BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter,
            IntegerValue::Signed(value),
        ) => {
            if bits < 128 {
                let minimum = -(1_i128 << (bits - 1));
                let maximum = (1_i128 << (bits - 1)) - 1;
                if !(minimum..=maximum).contains(&value) {
                    return Err("signed enumerator does not fit its representation".into());
                }
            }
            Ok(IntegerValue::Signed(value))
        }
        (
            BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter,
            IntegerValue::Unsigned(value),
        ) => {
            let value = i128::try_from(value)
                .map_err(|_| Arc::from("enumerator does not fit its signed representation"))?;
            checked_integer_value(IntegerValue::Signed(value), base)
        }
        (
            BaseTypeEncoding::Boolean
            | BaseTypeEncoding::Unsigned
            | BaseTypeEncoding::UnsignedCharacter,
            IntegerValue::Unsigned(value),
        ) => {
            if bits < 128 && value >= 1_u128 << bits {
                return Err("unsigned enumerator does not fit its representation".into());
            }
            if matches!(base.encoding, BaseTypeEncoding::Boolean) && value > 1 {
                return Err("boolean enumerator is neither zero nor one".into());
            }
            Ok(IntegerValue::Unsigned(value))
        }
        (
            BaseTypeEncoding::Boolean
            | BaseTypeEncoding::Unsigned
            | BaseTypeEncoding::UnsignedCharacter,
            IntegerValue::Signed(value),
        ) => {
            let value = u128::try_from(value)
                .map_err(|_| Arc::from("negative enumerator has an unsigned representation"))?;
            checked_integer_value(IntegerValue::Unsigned(value), base)
        }
        (BaseTypeEncoding::Floating, _) => {
            Err("enumerator representation is floating-point".into())
        }
    }
}

fn decode_integer_value(
    base: &BaseType,
    bytes: &[u8],
    byte_order: ByteOrder,
) -> std::result::Result<IntegerValue, Arc<str>> {
    let expected = usize::try_from(base.byte_size)
        .map_err(|_| Arc::from("integer byte size does not fit host usize"))?;
    if bytes.len() != expected {
        return Err("integer storage size mismatch".into());
    }
    let bits = integer_bit_width(base)?;
    let raw = unsigned_value(bytes, byte_order).map_err(|reason| Arc::from(reason.to_string()))?
        & low_bits_mask(usize::try_from(bits).expect("bit width fits usize"));
    let value = match base.encoding {
        BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter => {
            let signed = if bits == 128 {
                raw.cast_signed()
            } else {
                let shift = 128 - bits;
                (raw << shift).cast_signed() >> shift
            };
            IntegerValue::Signed(signed)
        }
        BaseTypeEncoding::Boolean
        | BaseTypeEncoding::Unsigned
        | BaseTypeEncoding::UnsignedCharacter => IntegerValue::Unsigned(raw),
        BaseTypeEncoding::Floating => {
            return Err("integer representation is floating-point".into());
        }
    };
    checked_integer_value(value, base)
}

fn enumeration_constant(
    value: gimli::AttributeValue<Reader<'_>>,
    base: &BaseType,
    byte_order: ByteOrder,
) -> std::result::Result<IntegerValue, Arc<str>> {
    let signed = matches!(
        base.encoding,
        BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter
    );
    let value = match value {
        gimli::AttributeValue::Data1(value) if signed => {
            IntegerValue::Signed(i128::from(value.cast_signed()))
        }
        gimli::AttributeValue::Data2(value) if signed => {
            IntegerValue::Signed(i128::from(value.cast_signed()))
        }
        gimli::AttributeValue::Data4(value) if signed => {
            IntegerValue::Signed(i128::from(value.cast_signed()))
        }
        gimli::AttributeValue::Data8(value) if signed => {
            IntegerValue::Signed(i128::from(value.cast_signed()))
        }
        gimli::AttributeValue::Data16(value) if signed => IntegerValue::Signed(value.cast_signed()),
        gimli::AttributeValue::Data1(value) => IntegerValue::Unsigned(u128::from(value)),
        gimli::AttributeValue::Data2(value) => IntegerValue::Unsigned(u128::from(value)),
        gimli::AttributeValue::Data4(value) => IntegerValue::Unsigned(u128::from(value)),
        gimli::AttributeValue::Data8(value) | gimli::AttributeValue::Udata(value) => {
            IntegerValue::Unsigned(u128::from(value))
        }
        gimli::AttributeValue::Data16(value) => IntegerValue::Unsigned(value),
        gimli::AttributeValue::Sdata(value) => IntegerValue::Signed(i128::from(value)),
        gimli::AttributeValue::Block(value) => {
            let bytes = value
                .to_slice()
                .map_err(|error| Arc::from(error.to_string()))?;
            return decode_integer_value(base, bytes.as_ref(), byte_order);
        }
        _ => return Err("enumerator constant has an unsupported form".into()),
    };
    checked_integer_value(value, base)
}

fn read_uleb128_u128(bytes: &[u8], cursor: &mut usize) -> std::result::Result<u128, Arc<str>> {
    let mut value = 0_u128;
    let mut shift = 0_u32;
    loop {
        let byte = *bytes
            .get(*cursor)
            .ok_or_else(|| Arc::from("truncated unsigned discriminant LEB128"))?;
        *cursor = cursor
            .checked_add(1)
            .ok_or_else(|| Arc::from("discriminant-list cursor overflows"))?;
        let payload = u128::from(byte & 0x7f);
        if shift >= 128 {
            if payload != 0 {
                return Err("unsigned discriminant LEB128 exceeds 128 bits".into());
            }
        } else {
            let available = 128 - shift;
            if available < 7 && payload >= 1_u128 << available {
                return Err("unsigned discriminant LEB128 exceeds 128 bits".into());
            }
            value |= payload << shift;
        }
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift = shift
            .checked_add(7)
            .ok_or_else(|| Arc::from("unsigned discriminant LEB128 shift overflows"))?;
        if shift > 133 {
            return Err("unsigned discriminant LEB128 is overlong".into());
        }
    }
}

fn read_sleb128_i128(bytes: &[u8], cursor: &mut usize) -> std::result::Result<i128, Arc<str>> {
    let mut value = 0_u128;
    let mut shift = 0_u32;
    loop {
        let byte = *bytes
            .get(*cursor)
            .ok_or_else(|| Arc::from("truncated signed discriminant LEB128"))?;
        *cursor = cursor
            .checked_add(1)
            .ok_or_else(|| Arc::from("discriminant-list cursor overflows"))?;
        let payload = u128::from(byte & 0x7f);
        if shift < 128 {
            let available = 128 - shift;
            value |= (payload & low_bits_mask(usize::try_from(available.min(7)).unwrap())) << shift;
        }
        if byte & 0x80 == 0 {
            let negative = byte & 0x40 != 0;
            if shift >= 128 {
                let canonical = if negative {
                    payload == 0x7f
                } else {
                    payload == 0
                };
                if !canonical {
                    return Err("signed discriminant LEB128 exceeds 128 bits".into());
                }
            } else {
                let consumed = shift + 7;
                if negative && consumed < 128 {
                    value |= u128::MAX << consumed;
                } else if consumed > 128 {
                    let used = 128 - shift;
                    let high = payload >> used;
                    let expected = if negative {
                        low_bits_mask(usize::try_from(7 - used).unwrap())
                    } else {
                        0
                    };
                    if high != expected {
                        return Err("signed discriminant LEB128 exceeds 128 bits".into());
                    }
                }
            }
            return Ok(value.cast_signed());
        }
        shift = shift
            .checked_add(7)
            .ok_or_else(|| Arc::from("signed discriminant LEB128 shift overflows"))?;
        if shift > 133 {
            return Err("signed discriminant LEB128 is overlong".into());
        }
    }
}

fn read_discriminant_leb128(
    bytes: &[u8],
    cursor: &mut usize,
    base: &BaseType,
) -> std::result::Result<IntegerValue, Arc<str>> {
    let value = if matches!(
        base.encoding,
        BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter
    ) {
        IntegerValue::Signed(read_sleb128_i128(bytes, cursor)?)
    } else {
        IntegerValue::Unsigned(read_uleb128_u128(bytes, cursor)?)
    };
    checked_integer_value(value, base)
}

fn copy_variant_selection(
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    representation: &BaseType,
    byte_order: ByteOrder,
    budget: &mut VariantMetadataBudget,
) -> std::result::Result<VariantSelection, VariantMetadataError> {
    let exact = entry.attr_value(gimli::DW_AT_discr_value);
    let list = entry.attr_value(gimli::DW_AT_discr_list);
    if exact.is_some() && list.is_some() {
        return Err(VariantMetadataError::Malformed(
            "variant has both DW_AT_discr_value and DW_AT_discr_list".into(),
        ));
    }
    if let Some(value) = exact {
        budget.consume()?;
        return enumeration_constant(value, representation, byte_order)
            .map(|value| VariantSelection::Selectors(Arc::from([VariantSelector::Value(value)])))
            .map_err(VariantMetadataError::Malformed);
    }
    let Some(list) = list else {
        return Ok(VariantSelection::Default);
    };
    let gimli::AttributeValue::Block(list) = list else {
        return Err(VariantMetadataError::Malformed(
            "DW_AT_discr_list does not use a block form".into(),
        ));
    };
    let bytes = list
        .to_slice()
        .map_err(|error| VariantMetadataError::Malformed(error.to_string().into()))?;
    parse_discriminant_list(bytes.as_ref(), representation, budget)
}

fn parse_discriminant_list(
    bytes: &[u8],
    representation: &BaseType,
    budget: &mut VariantMetadataBudget,
) -> std::result::Result<VariantSelection, VariantMetadataError> {
    if bytes.is_empty() {
        return Err(VariantMetadataError::Malformed(
            "DW_AT_discr_list is empty".into(),
        ));
    }
    let mut cursor = 0_usize;
    let mut selectors = Vec::new();
    while cursor < bytes.len() {
        budget.consume()?;
        let descriptor = bytes[cursor];
        cursor += 1;
        let low = read_discriminant_leb128(bytes, &mut cursor, representation)?;
        let selector = match descriptor {
            value if value == gimli::DW_DSC_label.0 => VariantSelector::Value(low),
            value if value == gimli::DW_DSC_range.0 => {
                let high = read_discriminant_leb128(bytes, &mut cursor, representation)?;
                VariantSelector::Range { low, high }
            }
            _ => {
                return Err(VariantMetadataError::Malformed(
                    format!("DW_AT_discr_list has unknown descriptor {descriptor:#x}").into(),
                ));
            }
        };
        selectors.push(selector);
    }
    Ok(VariantSelection::Selectors(selectors.into()))
}

fn compare_integer_values(
    left: IntegerValue,
    right: IntegerValue,
) -> std::result::Result<std::cmp::Ordering, Arc<str>> {
    match (left, right) {
        (IntegerValue::Signed(left), IntegerValue::Signed(right)) => Ok(left.cmp(&right)),
        (IntegerValue::Unsigned(left), IntegerValue::Unsigned(right)) => Ok(left.cmp(&right)),
        _ => Err("variant selectors mix signed and unsigned values".into()),
    }
}

fn variant_selection_matches(
    selection: &VariantSelection,
    value: IntegerValue,
) -> std::result::Result<bool, Arc<str>> {
    let VariantSelection::Selectors(selectors) = selection else {
        return Ok(false);
    };
    for selector in selectors.iter() {
        let matches = match *selector {
            VariantSelector::Value(expected) => expected == value,
            VariantSelector::Range { low, high } => {
                !compare_integer_values(value, low)?.is_lt()
                    && !compare_integer_values(value, high)?.is_gt()
            }
        };
        if matches {
            return Ok(true);
        }
    }
    Ok(false)
}

fn selected_variant_index(
    variants: &[Variant],
    value: IntegerValue,
) -> std::result::Result<Option<usize>, Arc<str>> {
    let mut selected = None;
    let mut default = None;
    for (index, variant) in variants.iter().enumerate() {
        match &variant.selection {
            VariantSelection::Default => default = Some(index),
            VariantSelection::Selectors(_) => {
                if variant_selection_matches(&variant.selection, value)? {
                    selected = Some(index);
                }
            }
        }
    }
    Ok(selected.or(default))
}

fn validate_variant_selections(variants: &[Variant]) -> std::result::Result<(), Arc<str>> {
    let mut default_count = 0_usize;
    let mut ranges = Vec::new();
    for (variant_index, variant) in variants.iter().enumerate() {
        match &variant.selection {
            VariantSelection::Default => default_count += 1,
            VariantSelection::Selectors(selectors) => {
                if selectors.is_empty() {
                    return Err("explicit variant selector list is empty".into());
                }
                for selector in selectors.iter() {
                    let (low, high) = match *selector {
                        VariantSelector::Value(value) => (value, value),
                        VariantSelector::Range { low, high } => (low, high),
                    };
                    if compare_integer_values(low, high)?.is_gt() {
                        return Err("variant discriminator range is reversed".into());
                    }
                    ranges.push((low, high, variant_index));
                }
            }
        }
    }
    if default_count > 1 {
        return Err("variant part has multiple default variants".into());
    }
    ranges.sort_by(|left, right| {
        compare_integer_values(left.0, right.0).unwrap_or(std::cmp::Ordering::Equal)
    });
    for pair in ranges.windows(2) {
        if !compare_integer_values(pair[0].1, pair[1].0)?.is_lt() {
            return Err("variant discriminator selectors overlap".into());
        }
    }
    Ok(())
}

fn zig_optional_payload_name(name: &str) -> Option<&str> {
    name.strip_prefix('?').filter(|payload| !payload.is_empty())
}

fn zig_error_union_type_names(name: &str) -> Option<(&str, &str)> {
    let (error, payload) = name.split_once('!')?;
    if payload.is_empty()
        || !(error == "anyerror" || error.starts_with("error{") && error.ends_with('}'))
    {
        return None;
    }
    Some((error, payload))
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
                // Use the shared constant/encoding classifiers so a base type
                // encoded with `DW_FORM_data16` is recognized here too. Any form
                // this backend cannot use is simply skipped for typed evaluation.
                let ByteSize::Constant(byte_size) = byte_size_attribute(entry) else {
                    continue;
                };
                let Ok(raw_encoding) = base_type_encoding(entry) else {
                    continue;
                };
                let encoding = gimli::DwAte(raw_encoding);
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
        type_signatures: &'a TypeSignatures,
        image: ModuleImageId,
        byte_order: ByteOrder,
    ) -> Self {
        let mut die_offsets = Vec::with_capacity(units.len());
        let mut unit_languages = Vec::with_capacity(units.len());
        let mut zig_units = Vec::with_capacity(units.len());
        let mut type_definitions = HashMap::new();
        let mut ambiguous_type_declarations = HashSet::new();
        for (unit_index, unit) in units.iter().enumerate() {
            let mut offsets = HashSet::new();
            let mut language = None;
            let mut zig_producer = false;
            let mut first = true;
            let mut entries = unit.entries();
            while let Ok(Some(entry)) = entries.next_dfs() {
                offsets.insert(entry.offset().0);
                if first {
                    first = false;
                    language = match entry.attr_value(gimli::DW_AT_language) {
                        Some(gimli::AttributeValue::Language(language)) => Some(language),
                        _ => None,
                    };
                    zig_producer = entry
                        .attr_value(gimli::DW_AT_producer)
                        .and_then(|value| dwarf.attr_string(unit, value).ok())
                        .is_some_and(|producer| producer.to_string_lossy().starts_with("zig "));
                }
                if !is_type_die_tag(entry.tag()) {
                    continue;
                }
                let Ok(Some(declaration)) = die_reference_with_signatures(
                    entry.attr_value(gimli::DW_AT_specification),
                    unit_index,
                    units,
                    type_signatures,
                ) else {
                    continue;
                };
                let definition = DieKey {
                    unit: unit_index,
                    offset: entry.offset().0,
                };
                if type_definitions
                    .insert(declaration, definition)
                    .is_some_and(|existing| existing != definition)
                {
                    ambiguous_type_declarations.insert(declaration);
                }
            }
            die_offsets.push(offsets);
            unit_languages.push(language);
            zig_units.push(zig_producer);
        }
        Self {
            dwarf,
            units,
            type_signatures,
            image,
            by_die: HashMap::new(),
            type_definitions,
            ambiguous_type_declarations,
            entries: Vec::new(),
            die_offsets,
            unit_languages,
            zig_units,
            explicit_names: HashSet::new(),
            resolution_depth: 0,
            byte_order,
            limit_type: None,
            dynamic_record_layouts: HashMap::new(),
            record_member_declarations: Vec::new(),
            symbolic_names: 0,
        }
    }

    fn variable_type(
        &mut self,
        unit_index: usize,
        value: Option<gimli::AttributeValue<Reader<'data>>>,
    ) -> TypeResolution {
        let key = match die_reference_with_signatures(
            value,
            unit_index,
            self.units,
            self.type_signatures,
        ) {
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
        match self.canonical_type_key(key) {
            Ok(canonical) if canonical != key => {
                let id = self.resolve(canonical);
                self.by_die.insert(key, id);
                return id;
            }
            Ok(_) => {}
            Err(reason) => {
                if self.entries.len() >= MAX_TYPES {
                    return self.type_limit(key);
                }
                let id = TypeId::new(
                    u32::try_from(self.entries.len()).expect("bounded type count fits u32"),
                );
                self.by_die.insert(key, id);
                self.entries.push(TypeEntry::Malformed(reason));
                return id;
            }
        }
        if self.entries.len() >= MAX_TYPES {
            return self.type_limit(key);
        }
        let id =
            TypeId::new(u32::try_from(self.entries.len()).expect("bounded type count fits u32"));
        self.by_die.insert(key, id);
        self.entries.push(TypeEntry::Building);
        if self.resolution_depth >= MAX_TYPE_RESOLUTION_DEPTH {
            self.entries[usize::try_from(id.get()).expect("type ID fits usize")] =
                TypeEntry::Malformed("type wrapper depth exceeds its limit".into());
            return id;
        }
        self.resolution_depth += 1;
        let entry = self.build(key, id);
        self.resolution_depth -= 1;
        self.entries[usize::try_from(id.get()).expect("type ID fits usize")] = entry;
        id
    }

    fn type_limit(&mut self, key: DieKey) -> TypeId {
        let id = if let Some(id) = self.limit_type {
            id
        } else {
            let id = TypeId::new(u32::try_from(self.entries.len()).expect("type count fits u32"));
            self.entries.push(TypeEntry::Malformed(
                "type graph exceeds its work limit".into(),
            ));
            self.limit_type = Some(id);
            id
        };
        self.by_die.insert(key, id);
        id
    }

    fn canonical_type_key(&self, key: DieKey) -> std::result::Result<DieKey, Arc<str>> {
        let mut current = key;
        let mut visited = HashSet::new();
        while visited.insert(current) {
            let unit = self
                .units
                .get(current.unit)
                .ok_or_else(|| Arc::from("type reference is outside loaded units"))?;
            if !self
                .die_offsets
                .get(current.unit)
                .is_some_and(|offsets| offsets.contains(&current.offset))
            {
                return Err("type reference does not identify a DIE".into());
            }
            let entry = unit
                .entry(gimli::UnitOffset(current.offset))
                .map_err(|error| Arc::from(error.to_string()))?;
            if !is_type_die_tag(entry.tag()) {
                return Err(format!("DW_AT_type target has non-type tag {:?}", entry.tag()).into());
            }
            if self.ambiguous_type_declarations.contains(&current) {
                return Err("type declaration has multiple definitions".into());
            }
            if let Some(definition) = self.type_definitions.get(&current).copied() {
                current = definition;
                continue;
            }
            let Some(signature) = entry.attr_value(gimli::DW_AT_signature) else {
                return Ok(current);
            };
            current = die_reference_with_signatures(
                Some(signature),
                current.unit,
                self.units,
                self.type_signatures,
            )
            .map_err(|error| Arc::from(error.to_string()))?
            .ok_or_else(|| Arc::from("type declaration signature has no definition"))?;
        }
        Err("type declaration/definition references form a cycle".into())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "type dispatch validates common attributes before one exhaustive tag match"
    )]
    fn build(&mut self, key: DieKey, id: TypeId) -> TypeEntry {
        let Some(unit) = self.units.get(key.unit) else {
            return TypeEntry::Malformed("type reference is outside loaded units".into());
        };
        // The offset must be a DIE boundary, not merely a byte offset that
        // happens to decode; otherwise a dangling reference could construct
        // convincing metadata from unrelated bytes. Every type resolution funnels
        // through here, so validating once covers direct references, pointer
        // targets, and wrapper chains alike.
        if !self
            .die_offsets
            .get(key.unit)
            .is_some_and(|offsets| offsets.contains(&key.offset))
        {
            return TypeEntry::Malformed("type reference does not identify a DIE".into());
        }
        let entry = match unit.entry(gimli::UnitOffset(key.offset)) {
            Ok(entry) => entry,
            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
        };
        // A `DW_AT_type` edge must name a type DIE. Reject a non-type target
        // before any attribute classification, so an oversized/dynamic size does
        // not mask the defect as a convincing unsupported type.
        if !is_type_die_tag(entry.tag()) {
            return TypeEntry::Malformed(
                format!("DW_AT_type target has non-type tag {:?}", entry.tag()).into(),
            );
        }
        let reference = TypeReference {
            image: self.image,
            id,
        };
        let origins = match origin_chain(self.units, key.unit, &entry) {
            Ok(origins) => origins,
            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
        };
        let explicit_name =
            match copy_name_with_origins(self.dwarf, self.units, unit, &entry, &origins) {
                Ok(name) => name,
                Err(error) => return TypeEntry::Malformed(error.to_string().into()),
            };
        if explicit_name.is_some() {
            self.explicit_names.insert(id);
        }
        // Validate the tag's mandatory attributes before classifying the byte
        // size. A dynamic or oversized size returns a terminal entry early, so
        // without this a defective encoding or missing target would be masked as
        // a convincing resolved type.
        if let Some(defect) = self.mandatory_attribute_defect(&entry, key.unit) {
            return TypeEntry::Malformed(defect);
        }
        // An absent address class defaults to zero. A present attribute that is
        // an oversized constant is valid but uninterpretable here; any other
        // non-constant form is defective. Silently treating either as the
        // default class could produce a convincing read using semantics the
        // producer never specified.
        let address_class = match resolve_address_class(&entry, reference, explicit_name.clone()) {
            Ok(address_class) => address_class,
            Err(resolved) => return *resolved,
        };
        let explicit_size = match resolve_explicit_size(&entry, reference, explicit_name.clone()) {
            Ok(size) => size,
            Err(resolved) => return *resolved,
        };
        let pointer_size = explicit_size
            .or_else(|| (address_class == 0).then_some(u64::from(unit.encoding().address_size)));

        match entry.tag() {
            gimli::DW_TAG_base_type => {
                Self::build_base_type(&entry, reference, explicit_name, explicit_size)
            }
            gimli::DW_TAG_enumeration_type => self.build_enumeration_type(
                &entry,
                key.unit,
                reference,
                explicit_name,
                explicit_size,
            ),
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
            gimli::DW_TAG_array_type => {
                self.build_array_type(&entry, key.unit, reference, explicit_name, explicit_size)
            }
            gimli::DW_TAG_structure_type
                if explicit_name.as_deref().is_some_and(is_slice_type_name) =>
            {
                self.build_slice_type(&entry, key.unit, reference, explicit_name, explicit_size)
            }
            gimli::DW_TAG_structure_type | gimli::DW_TAG_class_type => {
                self.build_record_type(&entry, key.unit, reference, explicit_name, explicit_size)
            }
            gimli::DW_TAG_union_type => {
                self.build_union_type(&entry, key.unit, reference, explicit_name, explicit_size)
            }
            gimli::DW_TAG_typedef
            | gimli::DW_TAG_template_alias
            | gimli::DW_TAG_const_type
            | gimli::DW_TAG_volatile_type
            | gimli::DW_TAG_restrict_type
            | gimli::DW_TAG_atomic_type
            | gimli::DW_TAG_immutable_type
            | gimli::DW_TAG_packed_type
            | gimli::DW_TAG_shared_type => {
                self.build_wrapper_type(&entry, key.unit, reference, explicit_name, explicit_size)
            }
            gimli::DW_TAG_unspecified_type => TypeEntry::Resolved(TypeInfo {
                reference,
                name: explicit_name.unwrap_or_else(|| Arc::from("void")),
                byte_size: explicit_size,
                kind: TypeKind::Unspecified,
            }),
            // A non-type tag was already rejected at the top of `build`, so any
            // remaining tag is a type this backend does not model; surface it as
            // opaque rather than defective.
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

    fn record_accessibility(
        entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
        record_kind: RecordKind,
    ) -> std::result::Result<Accessibility, Arc<str>> {
        match entry.attr_value(gimli::DW_AT_accessibility) {
            Some(gimli::AttributeValue::Accessibility(value))
                if value == gimli::DW_ACCESS_public =>
            {
                Ok(Accessibility::Public)
            }
            Some(gimli::AttributeValue::Accessibility(value))
                if value == gimli::DW_ACCESS_protected =>
            {
                Ok(Accessibility::Protected)
            }
            Some(gimli::AttributeValue::Accessibility(value))
                if value == gimli::DW_ACCESS_private =>
            {
                Ok(Accessibility::Private)
            }
            None if record_kind == RecordKind::Class => Ok(Accessibility::Private),
            None => Ok(Accessibility::Public),
            Some(_) => Err("record accessibility has an invalid encoding".into()),
        }
    }

    fn record_byte_layout(
        entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    ) -> RecordMemberLayout {
        let Some(attribute) = entry.attr(gimli::DW_AT_data_member_location) else {
            return RecordMemberLayout::Runtime;
        };
        attribute
            .udata_value()
            .map_or(RecordMemberLayout::Runtime, RecordMemberLayout::ByteOffset)
    }

    /// Reports a defect in a tag's mandatory attributes, independent of the byte
    /// size. Validating these before the size classification ensures a dynamic
    /// or oversized size cannot mask a missing encoding or target. Returns `None`
    /// when the tag's required attributes are present and well-formed.
    fn mandatory_attribute_defect(
        &self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> Option<Arc<str>> {
        match entry.tag() {
            gimli::DW_TAG_base_type => base_type_encoding(entry).err(),
            gimli::DW_TAG_reference_type | gimli::DW_TAG_rvalue_reference_type => self
                .target_defect(
                    entry,
                    unit_index,
                    "reference type",
                    TargetRequirement::Required,
                ),
            gimli::DW_TAG_typedef | gimli::DW_TAG_template_alias => {
                let declaration = match strict_flag(entry, gimli::DW_AT_declaration) {
                    Ok(declaration) => declaration,
                    Err(reason) => return Some(reason),
                };
                self.target_defect(
                    entry,
                    unit_index,
                    "named type",
                    named_type_target_requirement(declaration),
                )
            }
            gimli::DW_TAG_const_type
            | gimli::DW_TAG_volatile_type
            | gimli::DW_TAG_restrict_type
            | gimli::DW_TAG_atomic_type
            | gimli::DW_TAG_immutable_type
            | gimli::DW_TAG_array_type
            | gimli::DW_TAG_coarray_type
            | gimli::DW_TAG_set_type
            | gimli::DW_TAG_file_type
            | gimli::DW_TAG_dynamic_type
            | gimli::DW_TAG_ptr_to_member_type
            | gimli::DW_TAG_packed_type
            | gimli::DW_TAG_shared_type => {
                self.target_defect(entry, unit_index, "type", TargetRequirement::Required)
            }
            // A pointer or other type DIE may carry an optional `DW_AT_type`
            // (e.g. `void *`). If present, it must still name a real type DIE; a
            // dangling or non-type target is a defect even though absence is fine.
            _ => self.target_defect(entry, unit_index, "type", TargetRequirement::Optional),
        }
    }

    /// Reports a defect in a `DW_AT_type` target. Verifying the target here,
    /// before size classification can early-return an opaque entry, prevents a
    /// dangling or non-type edge from being masked as a convincing unsupported
    /// type. A `Required` target must be present; an `Optional` one may be absent
    /// but, when present, must still name a real type DIE.
    fn target_defect(
        &self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        kind: &str,
        requirement: TargetRequirement,
    ) -> Option<Arc<str>> {
        match die_reference_with_signatures(
            entry.attr_value(gimli::DW_AT_type),
            unit_index,
            self.units,
            self.type_signatures,
        ) {
            Ok(Some(key)) => {
                // The offset must be a DIE boundary, not merely a byte offset
                // that happens to decode; otherwise a dangling reference could
                // construct convincing metadata from unrelated bytes.
                let target = self
                    .die_offsets
                    .get(key.unit)
                    .filter(|offsets| offsets.contains(&key.offset))
                    .and_then(|_| self.units.get(key.unit))
                    .and_then(|unit| unit.entry(gimli::UnitOffset(key.offset)).ok());
                match target {
                    None => Some(format!("{kind} target does not identify a DIE").into()),
                    Some(target) if !is_type_die_tag(target.tag()) => {
                        Some(format!("{kind} target has non-type tag {:?}", target.tag()).into())
                    }
                    Some(_) => None,
                }
            }
            Ok(None) => match requirement {
                TargetRequirement::Required => Some(format!("{kind} has no target").into()),
                TargetRequirement::Optional => None,
            },
            Err(error) => Some(error.to_string().into()),
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
        if byte_size == 0 {
            // Zig models its storage-less `void` payload as a zero-byte signed
            // base type. It is not a scalar and must not poison a containing
            // aggregate, so normalize that compiler representation to the
            // platform-neutral unspecified type.
            if name.as_ref() == "void" {
                return TypeEntry::Resolved(TypeInfo {
                    reference,
                    name,
                    byte_size: Some(0),
                    kind: TypeKind::Unspecified,
                });
            }
            // A zero-width scalar is defective regardless of its encoding; reject
            // it before the encoding branch so an unsupported encoding cannot
            // mask the malformed size.
            return TypeEntry::Malformed("base type has a zero byte size".into());
        }
        let raw_encoding = match base_type_encoding(entry) {
            Ok(raw_encoding) => raw_encoding,
            Err(reason) => return TypeEntry::Malformed(reason),
        };
        let encoding = match gimli::DwAte(raw_encoding) {
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
            bit_size: match entry.attr(gimli::DW_AT_bit_size) {
                None => None,
                Some(attribute) => match unsigned_constant(attribute) {
                    UnsignedConstant::Value(0) => {
                        return TypeEntry::Malformed("base type has a zero bit size".into());
                    }
                    UnsignedConstant::Value(bit_size)
                        if bit_size <= byte_size.saturating_mul(8) =>
                    {
                        Some(bit_size)
                    }
                    UnsignedConstant::Value(_) | UnsignedConstant::Oversized => {
                        return TypeEntry::Malformed(
                            "base type bit size exceeds its byte storage".into(),
                        );
                    }
                    UnsignedConstant::NonConstant => {
                        return TypeEntry::Malformed(
                            "DW_AT_bit_size is not an unsigned integer constant".into(),
                        );
                    }
                },
            },
        };
        TypeEntry::Resolved(TypeInfo {
            reference,
            name,
            byte_size: Some(byte_size),
            kind: TypeKind::Base(base),
        })
    }

    fn resolved_integer_base(&self, id: TypeId) -> std::result::Result<BaseType, Arc<str>> {
        let mut current = id;
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(current) {
                return Err("enumeration underlying type contains a wrapper cycle".into());
            }
            let entry = self
                .entries
                .get(usize::try_from(current.get()).expect("type ID fits usize"))
                .ok_or_else(|| Arc::from("enumeration underlying type is outside the arena"))?;
            let info = match entry {
                TypeEntry::Resolved(info) => info,
                TypeEntry::Malformed(reason) => return Err(Arc::clone(reason)),
                TypeEntry::Building => {
                    return Err("enumeration underlying type did not finish building".into());
                }
            };
            match &info.kind {
                TypeKind::Base(base) if !matches!(base.encoding, BaseTypeEncoding::Floating) => {
                    return Ok(base.clone());
                }
                TypeKind::Enumeration { representation, .. } => {
                    return Ok(representation.clone());
                }
                TypeKind::Modified { target, .. }
                | TypeKind::Named {
                    target: Some(target),
                    ..
                } => {
                    current = target.id;
                }
                TypeKind::Base(_) => {
                    return Err("enumeration underlying type is not integral".into());
                }
                _ => return Err("enumeration underlying type is not an integer type".into()),
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "enumeration normalization validates representation and ordered symbols together"
    )]
    fn build_enumeration_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
    ) -> TypeEntry {
        let name = explicit_name.unwrap_or_else(|| {
            Arc::from(format!("<anonymous enumeration@0x{:x}>", entry.offset().0))
        });
        let underlying = match self.target(entry, unit_index) {
            Ok(underlying) => underlying,
            Err(reason) => return TypeEntry::Malformed(reason),
        };
        let mut representation = if let Some(underlying) = underlying {
            match self.resolved_integer_base(underlying.id) {
                Ok(base) => base,
                Err(reason) => return TypeEntry::Malformed(reason),
            }
        } else {
            let Some(byte_size) = explicit_size else {
                return TypeEntry::Malformed(
                    "enumeration has neither an underlying type nor a byte size".into(),
                );
            };
            if byte_size == 0 {
                return TypeEntry::Malformed("enumeration has a zero byte size".into());
            }
            let Ok(raw_encoding) = base_type_encoding(entry) else {
                return TypeEntry::Malformed(
                    "enumeration without an underlying type has no encoding".into(),
                );
            };
            let encoding = match gimli::DwAte(raw_encoding) {
                gimli::DW_ATE_boolean => BaseTypeEncoding::Boolean,
                gimli::DW_ATE_signed => BaseTypeEncoding::Signed,
                gimli::DW_ATE_signed_char => BaseTypeEncoding::SignedCharacter,
                gimli::DW_ATE_unsigned => BaseTypeEncoding::Unsigned,
                gimli::DW_ATE_unsigned_char => BaseTypeEncoding::UnsignedCharacter,
                _ => {
                    return TypeEntry::Malformed(
                        "enumeration encoding is not an integral encoding".into(),
                    );
                }
            };
            BaseType {
                name: Arc::clone(&name),
                base_name: Arc::clone(&name),
                encoding,
                byte_size,
                bit_size: None,
            }
        };
        let byte_size = explicit_size.unwrap_or(representation.byte_size);
        if byte_size == 0 {
            return TypeEntry::Malformed("enumeration has a zero byte size".into());
        }
        if byte_size != representation.byte_size {
            return TypeEntry::Malformed(
                "enumeration byte size differs from its underlying type".into(),
            );
        }
        if let Some(attribute) = entry.attr(gimli::DW_AT_encoding) {
            let enum_encoding = match unsigned_constant(attribute) {
                UnsignedConstant::Value(value) => u8::try_from(value).ok(),
                UnsignedConstant::Oversized | UnsignedConstant::NonConstant => None,
            };
            let compatible = enum_encoding.is_some_and(|encoding| {
                matches!(
                    (gimli::DwAte(encoding), representation.encoding),
                    (gimli::DW_ATE_boolean, BaseTypeEncoding::Boolean)
                        | (
                            gimli::DW_ATE_signed | gimli::DW_ATE_signed_char,
                            BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter
                        )
                        | (
                            gimli::DW_ATE_unsigned | gimli::DW_ATE_unsigned_char,
                            BaseTypeEncoding::Unsigned | BaseTypeEncoding::UnsignedCharacter
                        )
                )
            });
            if !compatible {
                return TypeEntry::Malformed(
                    "enumeration encoding differs from its underlying type".into(),
                );
            }
        }
        if let Some(attribute) = entry.attr(gimli::DW_AT_bit_size) {
            representation.bit_size = match unsigned_constant(attribute) {
                UnsignedConstant::Value(0) => {
                    return TypeEntry::Malformed("enumeration has a zero bit size".into());
                }
                UnsignedConstant::Value(value) if value <= byte_size.saturating_mul(8) => {
                    Some(value)
                }
                UnsignedConstant::Value(_)
                | UnsignedConstant::Oversized
                | UnsignedConstant::NonConstant => {
                    return TypeEntry::Malformed(
                        "enumeration bit size is not valid for its byte storage".into(),
                    );
                }
            };
        }
        representation.name = Arc::clone(&name);

        let Some(unit) = self.units.get(unit_index) else {
            return TypeEntry::Malformed("enumeration type unit is unavailable".into());
        };
        let mut tree = match unit.entries_tree(Some(entry.offset())) {
            Ok(tree) => tree,
            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
        };
        let root = match tree.root() {
            Ok(root) => root,
            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
        };
        let mut enumerators = Vec::new();
        let mut children = root.children();
        loop {
            let child = match children.next() {
                Ok(Some(child)) => child,
                Ok(None) => break,
                Err(error) => return TypeEntry::Malformed(error.to_string().into()),
            };
            let child = child.entry();
            if child.tag() != gimli::DW_TAG_enumerator {
                return TypeEntry::Malformed(
                    format!(
                        "enumeration contains unsupported direct child {:?}",
                        child.tag()
                    )
                    .into(),
                );
            }
            if enumerators.len() >= MAX_RECORD_CHILDREN || self.symbolic_names >= MAX_SYMBOLIC_NAMES
            {
                return TypeEntry::Resolved(TypeInfo {
                    reference,
                    name,
                    byte_size: Some(byte_size),
                    kind: TypeKind::Opaque {
                        description: "enumerator metadata exceeds its resource limit".into(),
                    },
                });
            }
            self.symbolic_names += 1;
            let enumerator_name = match copy_name(self.dwarf, unit, child) {
                Ok(Some(name)) => name,
                Ok(None) => return TypeEntry::Malformed("enumerator has no name".into()),
                Err(error) => return TypeEntry::Malformed(error.to_string().into()),
            };
            let Some(value) = child.attr_value(gimli::DW_AT_const_value) else {
                return TypeEntry::Malformed("enumerator has no constant value".into());
            };
            let value = match enumeration_constant(value, &representation, self.byte_order) {
                Ok(value) => value,
                Err(reason) => return TypeEntry::Malformed(reason),
            };
            enumerators.push(Enumerator {
                name: enumerator_name,
                value,
            });
        }
        let scoped = match strict_flag(entry, gimli::DW_AT_enum_class) {
            Ok(scoped) => scoped,
            Err(reason) => return TypeEntry::Malformed(reason),
        };
        TypeEntry::Resolved(TypeInfo {
            reference,
            name,
            byte_size: Some(byte_size),
            kind: TypeKind::Enumeration {
                representation,
                underlying,
                enumerators: enumerators.into(),
                origin: EnumerationOrigin::Language,
                scoped,
            },
        })
    }

    fn target(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> std::result::Result<Option<TypeReference>, Arc<str>> {
        die_reference_with_signatures(
            entry.attr_value(gimli::DW_AT_type),
            unit_index,
            self.units,
            self.type_signatures,
        )
        .map(|key| {
            key.map(|key| TypeReference {
                image: self.image,
                id: self.resolve(key),
            })
        })
        .map_err(|error| error.to_string().into())
    }

    fn target_with_origins(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        chain: &[(usize, gimli::DebuggingInformationEntry<Reader<'data>>)],
    ) -> std::result::Result<Option<TypeReference>, Arc<str>> {
        let (owner, value) = entry
            .attr_value(gimli::DW_AT_type)
            .map(|value| (unit_index, value))
            .or_else(|| {
                chain.iter().find_map(|(origin_unit, origin)| {
                    origin
                        .attr_value(gimli::DW_AT_type)
                        .map(|value| (*origin_unit, value))
                })
            })
            .map_or((unit_index, None), |(owner, value)| (owner, Some(value)));
        die_reference_with_signatures(value, owner, self.units, self.type_signatures)
            .map(|key| {
                key.map(|key| TypeReference {
                    image: self.image,
                    id: self.resolve(key),
                })
            })
            .map_err(|error| error.to_string().into())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Go constant reconstruction keeps producer filtering, validation, budgets, and promotion together"
    )]
    fn populate_go_named_constants(&mut self) {
        let mut constants = HashMap::<TypeId, NamedConstantCollection>::new();
        for (unit_index, unit) in self.units.iter().enumerate() {
            let mut entries = unit.entries();
            let Ok(Some(root)) = entries.next_dfs() else {
                continue;
            };
            if !matches!(
                root.attr_value(gimli::DW_AT_language),
                Some(gimli::AttributeValue::Language(language)) if language == gimli::DW_LANG_Go
            ) {
                continue;
            }
            while let Ok(Some(entry)) = entries.next_dfs() {
                if entry.tag() != gimli::DW_TAG_constant {
                    continue;
                }
                let Ok(Some(key)) = die_reference_with_signatures(
                    entry.attr_value(gimli::DW_AT_type),
                    unit_index,
                    self.units,
                    self.type_signatures,
                ) else {
                    continue;
                };
                let target = self.resolve(key);
                let Some(TypeEntry::Resolved(info)) = self
                    .entries
                    .get(usize::try_from(target.get()).expect("type ID fits usize"))
                else {
                    continue;
                };
                let TypeKind::Base(base) = &info.kind else {
                    continue;
                };
                if !info.name.contains('.') || matches!(base.encoding, BaseTypeEncoding::Floating) {
                    continue;
                }
                let representation = base.clone();
                let enumerator = copy_name(self.dwarf, unit, entry)
                    .map_err(|error| Arc::from(error.to_string()))
                    .and_then(|name| name.ok_or_else(|| Arc::from("typed Go constant has no name")))
                    .and_then(|name| {
                        entry
                            .attr_value(gimli::DW_AT_const_value)
                            .ok_or_else(|| Arc::from("typed Go constant has no value"))
                            .and_then(|value| {
                                enumeration_constant(value, &representation, self.byte_order)
                            })
                            .map(|value| Enumerator { name, value })
                    });
                let symbolic_limit =
                    enumerator.is_ok() && self.symbolic_names >= MAX_SYMBOLIC_NAMES;
                if enumerator.is_ok() && !symbolic_limit {
                    self.symbolic_names += 1;
                }
                let collection = constants
                    .entry(target)
                    .or_insert_with(|| NamedConstantCollection::Enumerators(Vec::new()));
                if symbolic_limit {
                    *collection = NamedConstantCollection::Limit;
                    continue;
                }
                match (collection, enumerator) {
                    (NamedConstantCollection::Enumerators(values), Ok(enumerator))
                        if values.len() < MAX_RECORD_CHILDREN =>
                    {
                        values.push(enumerator);
                    }
                    (collection @ NamedConstantCollection::Enumerators(_), Ok(_)) => {
                        *collection = NamedConstantCollection::Limit;
                    }
                    (collection @ NamedConstantCollection::Enumerators(_), Err(reason)) => {
                        *collection = NamedConstantCollection::Malformed(reason);
                    }
                    (NamedConstantCollection::Malformed(_) | NamedConstantCollection::Limit, _) => {
                    }
                }
            }
        }

        for (target, collection) in constants {
            let index = usize::try_from(target.get()).expect("type ID fits usize");
            match collection {
                NamedConstantCollection::Malformed(reason) => {
                    self.entries[index] = TypeEntry::Malformed(reason);
                }
                NamedConstantCollection::Limit => {
                    let TypeEntry::Resolved(info) = &mut self.entries[index] else {
                        continue;
                    };
                    info.kind = TypeKind::Opaque {
                        description: "typed Go constant count exceeds its resource limit".into(),
                    };
                }
                NamedConstantCollection::Enumerators(enumerators) if !enumerators.is_empty() => {
                    let TypeEntry::Resolved(info) = &mut self.entries[index] else {
                        continue;
                    };
                    let TypeKind::Base(base) = &info.kind else {
                        continue;
                    };
                    let mut representation = base.clone();
                    representation.name = Arc::clone(&info.name);
                    info.kind = TypeKind::Enumeration {
                        representation,
                        underlying: None,
                        enumerators: enumerators.into(),
                        origin: EnumerationOrigin::NamedConstants,
                        scoped: false,
                    };
                }
                NamedConstantCollection::Enumerators(_) => {}
            }
        }
    }

    fn populate_record_member_declarations(
        &mut self,
        source_files: &mut Vec<SourceFile>,
        source_file_ids: &mut HashMap<PathBuf, SourceFileId>,
    ) {
        for metadata in self.record_member_declarations.clone() {
            let record = metadata.aggregate;
            let key = metadata.die;
            let declaration = (|| -> std::result::Result<Option<SourceLocation>, Arc<str>> {
                let unit = self
                    .units
                    .get(key.unit)
                    .ok_or_else(|| Arc::from("record member unit is unavailable"))?;
                let entry = unit
                    .entry(gimli::UnitOffset(key.offset))
                    .map_err(|error| Arc::from(error.to_string()))?;
                let chain = origin_chain(self.units, key.unit, &entry)
                    .map_err(|error| Arc::from(error.to_string()))?;
                declaration_with_origins(
                    self.dwarf,
                    self.units,
                    unit,
                    &entry,
                    &chain,
                    source_files,
                    source_file_ids,
                )
                .map_err(|error| Arc::from(error.to_string()))
            })();
            let declaration = match declaration {
                Ok(declaration) => declaration,
                Err(reason) => {
                    self.entries[usize::try_from(record.get()).expect("type ID fits usize")] =
                        TypeEntry::Malformed(reason);
                    continue;
                }
            };
            let Some(TypeEntry::Resolved(info)) = self
                .entries
                .get_mut(usize::try_from(record.get()).expect("type ID fits usize"))
            else {
                continue;
            };
            match (&mut info.kind, metadata.member) {
                (
                    TypeKind::Record { members, .. }
                    | TypeKind::Union { members, .. }
                    | TypeKind::Variant {
                        common_members: members,
                        ..
                    },
                    AggregateMemberPath::Direct(member),
                ) => {
                    let mut updated = members.to_vec();
                    if let Some(member) = updated.get_mut(member) {
                        member.declaration = declaration;
                        *members = updated.into();
                    }
                }
                (TypeKind::Variant { discriminant, .. }, AggregateMemberPath::Discriminant) => {
                    match discriminant.as_mut() {
                        VariantDiscriminant::Stored(member) => {
                            member.declaration = declaration;
                        }
                        VariantDiscriminant::TagType(_) => {}
                    }
                }
                (
                    TypeKind::Variant { variants, .. },
                    AggregateMemberPath::Variant { variant, member },
                ) => {
                    let mut updated_variants = variants.to_vec();
                    if let Some(variant) = updated_variants.get_mut(variant) {
                        let mut updated_members = variant.members.to_vec();
                        if let Some(member) = updated_members.get_mut(member) {
                            member.declaration = declaration;
                            variant.members = updated_members.into();
                            *variants = updated_variants.into();
                        }
                    }
                }
                _ => {}
            }
        }
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

    fn finalize_type_graph(&mut self) {
        for (index, entry) in self.entries.iter_mut().enumerate() {
            if matches!(entry, TypeEntry::Building) {
                *entry = TypeEntry::Malformed(
                    format!("type graph node {index} did not finish building").into(),
                );
            }
        }

        propagate_wrapper_sizes(&mut self.entries);

        self.reject_inline_storage_cycles();

        #[expect(
            clippy::needless_collect,
            reason = "name rendering borrows the complete graph immutably before names are replaced"
        )]
        let names = (0..self.entries.len())
            .map(|index| {
                let id = TypeId::new(u32::try_from(index).expect("bounded type count fits u32"));
                self.render_type_name(id, &mut HashSet::new())
            })
            .collect::<Vec<_>>();
        for (index, name) in names.into_iter().enumerate() {
            if self.explicit_names.contains(&TypeId::new(
                u32::try_from(index).expect("bounded type count fits u32"),
            )) {
                continue;
            }
            if let Some(TypeEntry::Resolved(info)) = self.entries.get_mut(index) {
                info.name = name;
            }
        }
    }

    fn reject_inline_storage_cycles(&mut self) {
        let description: Arc<str> = "type graph contains an inline-storage cycle".into();
        for node in inline_storage_cycle_nodes(&self.entries) {
            self.entries[node] = TypeEntry::Malformed(Arc::clone(&description));
        }
    }

    fn render_type_name(&self, id: TypeId, visiting: &mut HashSet<TypeId>) -> Arc<str> {
        if !visiting.insert(id) {
            return Arc::from(format!("<type #{}>", id.get()));
        }
        let Some(TypeEntry::Resolved(info)) = self
            .entries
            .get(usize::try_from(id.get()).expect("type ID fits usize"))
        else {
            visiting.remove(&id);
            return Arc::from(format!("<type #{}>", id.get()));
        };
        if self.explicit_names.contains(&id) {
            visiting.remove(&id);
            return Arc::clone(&info.name);
        }

        let rendered = match &info.kind {
            TypeKind::Pointer { target, .. } => target.as_ref().map_or_else(
                || Arc::from("void *"),
                |target| {
                    let target_name = self.render_type_name(target.id, visiting);
                    Arc::from(self.indirection_type_name(target.id, &target_name, "*"))
                },
            ),
            TypeKind::Reference { kind, target, .. } => {
                let target_name = self.render_type_name(target.id, visiting);
                Arc::from(self.indirection_type_name(
                    target.id,
                    &target_name,
                    if *kind == ReferenceKind::Lvalue {
                        "&"
                    } else {
                        "&&"
                    },
                ))
            }
            TypeKind::Array {
                element,
                dimensions,
            } => {
                let mut name = self.render_type_name(element.id, visiting).to_string();
                for dimension in dimensions.iter() {
                    use std::fmt::Write;
                    let _ = write!(name, "[{}]", dimension.count);
                }
                Arc::from(name)
            }
            TypeKind::Modified { modifier, target } => {
                let target_name = self.render_type_name(target.id, visiting);
                let target_is_indirection = self.modified_target_is_indirection(target.id);
                Arc::from(modifier_type_name(
                    *modifier,
                    &target_name,
                    target_is_indirection,
                ))
            }
            TypeKind::Named {
                target: Some(target),
                ..
            } => self.render_type_name(target.id, visiting),
            _ => Arc::clone(&info.name),
        };
        visiting.remove(&id);
        rendered
    }

    fn modified_target_is_indirection(&self, start: TypeId) -> bool {
        let mut current = start;
        let mut visited = HashSet::new();
        while visited.insert(current) {
            let Some(TypeEntry::Resolved(info)) = self
                .entries
                .get(usize::try_from(current.get()).expect("type ID fits usize"))
            else {
                return false;
            };
            match info.kind {
                TypeKind::Pointer { .. } | TypeKind::Reference { .. } => return true,
                TypeKind::Modified { target, .. } => current = target.id,
                _ => return false,
            }
        }
        false
    }

    fn indirection_type_name(&self, target: TypeId, target_name: &str, symbol: &str) -> String {
        let target_is_synthesized_array = !self.explicit_names.contains(&target)
            && self
                .entries
                .get(usize::try_from(target.get()).expect("type ID fits usize"))
                .is_some_and(|entry| {
                    matches!(
                        entry,
                        TypeEntry::Resolved(TypeInfo {
                            kind: TypeKind::Array { .. },
                            ..
                        })
                    )
                });
        if target_is_synthesized_array && let Some(suffix) = target_name.find('[') {
            return format!(
                "{} ({symbol}){}",
                target_name[..suffix].trim_end(),
                &target_name[suffix..]
            );
        }
        format!("{target_name} {symbol}")
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
            Ok(target) => target,
            Err(reason) => return TypeEntry::Malformed(reason),
        };
        let inherited_size = target
            .and_then(|target| {
                self.entries
                    .get(usize::try_from(target.id.get()).expect("type ID fits usize"))
            })
            .and_then(|entry| match entry {
                TypeEntry::Resolved(info) => info.byte_size,
                TypeEntry::Building | TypeEntry::Malformed(_) => None,
            });
        let byte_size = explicit_size.or(inherited_size);
        if matches!(
            entry.tag(),
            gimli::DW_TAG_typedef | gimli::DW_TAG_template_alias
        ) {
            let relationship = named_type_relationship(
                entry.tag(),
                self.unit_languages.get(unit_index).copied().flatten(),
                self.zig_units.get(unit_index).copied().unwrap_or(false),
            );
            return TypeEntry::Resolved(TypeInfo {
                reference,
                name: explicit_name.unwrap_or_else(|| {
                    target.map_or_else(
                        || Arc::from("<incomplete named type>"),
                        |target| self.target_name(target),
                    )
                }),
                byte_size,
                kind: TypeKind::Named {
                    target,
                    relationship,
                },
            });
        }
        let Some(target) = target else {
            return TypeEntry::Malformed("type modifier has no target".into());
        };
        let qualifier =
            type_modifier(entry.tag()).expect("modifier wrapper tags were matched by caller");
        let name = explicit_name
            .unwrap_or_else(|| Arc::from(format!("{qualifier:?} {}", self.target_name(target))));
        TypeEntry::Resolved(TypeInfo {
            reference,
            name,
            byte_size,
            kind: TypeKind::Modified {
                modifier: qualifier,
                target,
            },
        })
    }

    fn has_direct_variant_part(
        &self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> std::result::Result<bool, Arc<str>> {
        let unit = self
            .units
            .get(unit_index)
            .ok_or_else(|| Arc::from("aggregate type unit is unavailable"))?;
        let mut tree = unit
            .entries_tree(Some(entry.offset()))
            .map_err(|error| Arc::from(error.to_string()))?;
        let root = tree.root().map_err(|error| Arc::from(error.to_string()))?;
        let mut found = false;
        let mut children = root.children();
        while let Some(child) = children
            .next()
            .map_err(|error| Arc::from(error.to_string()))?
        {
            if child.entry().tag() == gimli::DW_TAG_variant_part {
                if found {
                    return Err("aggregate contains multiple direct variant parts".into());
                }
                found = true;
            }
        }
        Ok(found)
    }

    fn unit_is_zig(&self, unit_index: usize) -> bool {
        let Some(unit) = self.units.get(unit_index) else {
            return false;
        };
        let mut entries = unit.entries();
        let Ok(Some(root)) = entries.next_dfs() else {
            return false;
        };
        let Some(producer) = root.attr_value(gimli::DW_AT_producer) else {
            return false;
        };
        self.dwarf
            .attr_string(unit, producer)
            .is_ok_and(|producer| producer.to_string_lossy().contains("zig"))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Zig tagged-union recognition and remapping form one fail-closed structural validation"
    )]
    fn normalize_zig_tagged_union(
        &mut self,
        unit_index: usize,
        aggregate: TypeId,
        members: &[RecordMember],
        incomplete: bool,
    ) -> Option<std::result::Result<TypeKind, Arc<str>>> {
        if incomplete || !self.unit_is_zig(unit_index) || members.len() != 2 {
            return None;
        }
        let payload_index = members
            .iter()
            .position(|member| member.name.as_deref() == Some("payload"))?;
        let tag_index = members
            .iter()
            .position(|member| member.name.as_deref() == Some("tag"))?;
        let payload = &members[payload_index];
        let tag = &members[tag_index];
        let payload_info = self
            .entries
            .get(usize::try_from(payload.type_ref.id.get()).expect("type ID fits usize"))?;
        let tag_info = self
            .entries
            .get(usize::try_from(tag.type_ref.id.get()).expect("type ID fits usize"))?;
        let (
            TypeEntry::Resolved(TypeInfo {
                name: payload_name,
                kind:
                    TypeKind::Union {
                        members: payload_members,
                        incomplete: false,
                    },
                ..
            }),
            TypeEntry::Resolved(TypeInfo {
                name: tag_name,
                kind:
                    TypeKind::Enumeration {
                        enumerators,
                        origin: EnumerationOrigin::Language,
                        ..
                    },
                ..
            }),
        ) = (payload_info, tag_info)
        else {
            return None;
        };
        if !payload_name.ends_with(":Payload")
            || !tag_name.starts_with("@typeInfo(")
            || !tag_name.contains(".@\"union\".tag_type")
        {
            return None;
        }
        let payload_members = Arc::clone(payload_members);
        let enumerators = Arc::clone(enumerators);
        let payload_offset = match payload.layout {
            RecordMemberLayout::ByteOffset(offset) => offset,
            RecordMemberLayout::BitRange { .. } | RecordMemberLayout::Runtime => {
                return Some(Err(
                    "Zig tagged-union payload has a non-byte-static location".into(),
                ));
            }
        };
        let mut variants = Vec::with_capacity(enumerators.len());
        for enumerator in enumerators.iter() {
            let matching = payload_members
                .iter()
                .enumerate()
                .filter(|(_, member)| member.name.as_deref() == Some(enumerator.name.as_ref()))
                .collect::<Vec<_>>();
            let [(payload_member_index, payload_member)] = matching.as_slice() else {
                return Some(Err(format!(
                    "Zig tagged-union arm '{}' does not map uniquely to its payload union",
                    enumerator.name
                )
                .into()));
            };
            let payload_type = self
                .entries
                .get(usize::try_from(payload_member.type_ref.id.get()).expect("type ID fits usize"))
                .and_then(|entry| match entry {
                    TypeEntry::Resolved(info) => Some(&info.kind),
                    TypeEntry::Building | TypeEntry::Malformed(_) => None,
                });
            let variant_members = if matches!(payload_type, Some(TypeKind::Unspecified)) {
                Arc::from([])
            } else {
                let mut member = (*payload_member).clone();
                member.layout = match member.layout {
                    RecordMemberLayout::ByteOffset(offset) => {
                        let Some(offset) = payload_offset.checked_add(offset) else {
                            return Some(Err(
                                "Zig tagged-union payload member offset overflows".into()
                            ));
                        };
                        RecordMemberLayout::ByteOffset(offset)
                    }
                    RecordMemberLayout::BitRange {
                        bit_offset,
                        bit_size,
                    } => {
                        let Some(bit_offset) = payload_offset
                            .checked_mul(8)
                            .and_then(|offset| offset.checked_add(bit_offset))
                        else {
                            return Some(Err(
                                "Zig tagged-union payload bit offset overflows".into()
                            ));
                        };
                        RecordMemberLayout::BitRange {
                            bit_offset,
                            bit_size,
                        }
                    }
                    RecordMemberLayout::Runtime => {
                        return Some(Err(
                            "Zig tagged-union payload member has a runtime location".into(),
                        ));
                    }
                };
                if let Some(metadata) = self.record_member_declarations.iter().find(|metadata| {
                    metadata.aggregate == payload.type_ref.id
                        && matches!(
                            metadata.member,
                            AggregateMemberPath::Direct(index)
                                if index == *payload_member_index
                        )
                }) {
                    self.record_member_declarations
                        .push(AggregateMemberDeclaration {
                            aggregate,
                            member: AggregateMemberPath::Variant {
                                variant: variants.len(),
                                member: 0,
                            },
                            die: metadata.die,
                        });
                }
                Arc::from([member])
            };
            variants.push(Variant {
                name: Some(Arc::clone(&enumerator.name)),
                selection: VariantSelection::Selectors(Arc::from([VariantSelector::Value(
                    enumerator.value,
                )])),
                members: variant_members,
            });
        }
        for metadata in &mut self.record_member_declarations {
            if metadata.aggregate == aggregate
                && matches!(
                    metadata.member,
                    AggregateMemberPath::Direct(index) if index == tag_index
                )
            {
                metadata.member = AggregateMemberPath::Discriminant;
            }
        }
        self.record_member_declarations.retain(|metadata| {
            metadata.aggregate != aggregate
                || !matches!(
                    metadata.member,
                    AggregateMemberPath::Direct(index) if index == payload_index
                )
        });
        Some(Ok(TypeKind::Variant {
            storage: VariantStorageKind::Struct,
            common_members: Arc::from([]),
            bases: Arc::from([]),
            discriminant: Box::new(VariantDiscriminant::Stored(tag.clone())),
            variants: variants.into(),
            incomplete: false,
        }))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Zig optional/error-union recognition remaps layout, declarations, and dynamic expressions atomically"
    )]
    fn normalize_zig_optional_or_error_union(
        &mut self,
        unit_index: usize,
        aggregate: TypeId,
        name: &str,
        members: &[RecordMember],
        incomplete: bool,
    ) -> Option<std::result::Result<TypeKind, Arc<str>>> {
        if incomplete || !self.unit_is_zig(unit_index) || members.len() != 2 {
            return None;
        }
        let payload_index = members
            .iter()
            .position(|member| member.name.as_deref() == Some("payload"))?;
        let optional_payload = zig_optional_payload_name(name);
        let error_union_types = zig_error_union_type_names(name);
        let (discriminant_index, variants, duplicate_discriminant_member) =
            if let Some(expected_payload) = optional_payload {
                if self.target_name(members[payload_index].type_ref).as_ref() != expected_payload {
                    return None;
                }
                let some_index = members
                    .iter()
                    .position(|member| member.name.as_deref() == Some("some"))?;
                let some_type = self
                    .resolved_integer_base(members[some_index].type_ref.id)
                    .ok()?;
                if some_type.byte_size != 1
                    || !matches!(
                        some_type.encoding,
                        BaseTypeEncoding::Boolean
                            | BaseTypeEncoding::Unsigned
                            | BaseTypeEncoding::UnsignedCharacter
                    )
                {
                    return None;
                }
                (
                    some_index,
                    vec![
                        Variant {
                            name: Some("null".into()),
                            selection: VariantSelection::Selectors(Arc::from([
                                VariantSelector::Value(IntegerValue::Unsigned(0)),
                            ])),
                            members: Arc::from([]),
                        },
                        Variant {
                            name: Some("some".into()),
                            selection: VariantSelection::Selectors(Arc::from([
                                VariantSelector::Value(IntegerValue::Unsigned(1)),
                            ])),
                            members: Arc::from([members[payload_index].clone()]),
                        },
                    ],
                    false,
                )
            } else if let Some((expected_error, expected_payload)) = error_union_types {
                let error_index = members
                    .iter()
                    .position(|member| member.name.as_deref() == Some("error"))?;
                if self.target_name(members[error_index].type_ref).as_ref() != expected_error
                    || self.target_name(members[payload_index].type_ref).as_ref()
                        != expected_payload
                {
                    return None;
                }
                let error_type = self
                    .resolved_integer_base(members[error_index].type_ref.id)
                    .ok()?;
                if matches!(
                    error_type.encoding,
                    BaseTypeEncoding::Signed
                        | BaseTypeEncoding::SignedCharacter
                        | BaseTypeEncoding::Floating
                ) {
                    return None;
                }
                (
                    error_index,
                    vec![
                        Variant {
                            name: Some("success".into()),
                            selection: VariantSelection::Selectors(Arc::from([
                                VariantSelector::Value(IntegerValue::Unsigned(0)),
                            ])),
                            members: Arc::from([members[payload_index].clone()]),
                        },
                        Variant {
                            name: Some("error".into()),
                            selection: VariantSelection::Default,
                            members: Arc::from([members[error_index].clone()]),
                        },
                    ],
                    true,
                )
            } else {
                return None;
            };
        if let Err(reason) = validate_variant_selections(&variants) {
            return Some(Err(reason));
        }

        let payload_variant = usize::from(optional_payload.is_some());
        let declaration_snapshot = self.record_member_declarations.clone();
        for metadata in &mut self.record_member_declarations {
            if metadata.aggregate != aggregate {
                continue;
            }
            match metadata.member {
                AggregateMemberPath::Direct(index) if index == discriminant_index => {
                    metadata.member = AggregateMemberPath::Discriminant;
                }
                AggregateMemberPath::Direct(index) if index == payload_index => {
                    metadata.member = AggregateMemberPath::Variant {
                        variant: payload_variant,
                        member: 0,
                    };
                }
                _ => {}
            }
        }
        if duplicate_discriminant_member
            && let Some(metadata) = declaration_snapshot.iter().find(|metadata| {
                metadata.aggregate == aggregate
                    && matches!(
                        metadata.member,
                        AggregateMemberPath::Direct(index) if index == discriminant_index
                    )
            })
        {
            self.record_member_declarations
                .push(AggregateMemberDeclaration {
                    aggregate,
                    member: AggregateMemberPath::Variant {
                        variant: 1,
                        member: 0,
                    },
                    die: metadata.die,
                });
        }

        for (index, child) in [
            (
                payload_index,
                DynamicAggregateChild::VariantMember {
                    variant: payload_variant,
                    member: 0,
                },
            ),
            (discriminant_index, DynamicAggregateChild::Discriminant),
        ] {
            if let Some(expression) =
                self.dynamic_record_layouts
                    .remove(&DynamicAggregateLayoutKey {
                        aggregate,
                        child: DynamicAggregateChild::Member(index),
                    })
            {
                self.dynamic_record_layouts
                    .insert(DynamicAggregateLayoutKey { aggregate, child }, expression);
            }
        }
        if duplicate_discriminant_member
            && let Some(expression) = self
                .dynamic_record_layouts
                .get(&DynamicAggregateLayoutKey {
                    aggregate,
                    child: DynamicAggregateChild::Discriminant,
                })
                .cloned()
        {
            self.dynamic_record_layouts.insert(
                DynamicAggregateLayoutKey {
                    aggregate,
                    child: DynamicAggregateChild::VariantMember {
                        variant: 1,
                        member: 0,
                    },
                },
                expression,
            );
        }

        Some(Ok(TypeKind::Variant {
            storage: VariantStorageKind::Struct,
            common_members: Arc::from([]),
            bases: Arc::from([]),
            discriminant: Box::new(VariantDiscriminant::Stored(
                members[discriminant_index].clone(),
            )),
            variants: variants.into(),
            incomplete: false,
        }))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "variant members retain owner, location identity, access, and declaration provenance"
    )]
    fn build_variant_member(
        &mut self,
        child: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit: &gimli::Unit<Reader<'data>>,
        unit_index: usize,
        aggregate: TypeId,
        dynamic_child: DynamicAggregateChild,
        absent_byte_offset: Option<u64>,
        record_kind: RecordKind,
        declaration_path: AggregateMemberPath,
    ) -> std::result::Result<RecordMember, Arc<str>> {
        let chain = origin_chain(self.units, unit_index, child)
            .map_err(|error| Arc::from(error.to_string()))?;
        let target = self
            .target_with_origins(child, unit_index, &chain)?
            .ok_or_else(|| Arc::from("variant component has no type"))?;
        let name = copy_name_with_origins(self.dwarf, self.units, unit, child, &chain)
            .map_err(|error| Arc::from(error.to_string()))?;
        let layout = self.record_member_layout(child, target, absent_byte_offset)?;
        if layout == RecordMemberLayout::Runtime
            && let Some(expression) = self
                .copy_dynamic_record_layout(child, unit_index)
                .map_err(|error| Arc::from(error.to_string()))?
        {
            self.dynamic_record_layouts.insert(
                DynamicAggregateLayoutKey {
                    aggregate,
                    child: dynamic_child,
                },
                expression,
            );
        }
        self.record_member_declarations
            .push(AggregateMemberDeclaration {
                aggregate,
                member: declaration_path,
                die: DieKey {
                    unit: unit_index,
                    offset: child.offset().0,
                },
            });
        Ok(RecordMember {
            name,
            type_ref: target,
            layout,
            accessibility: Self::record_accessibility(child, record_kind)?,
            artificial: strict_flag(child, gimli::DW_AT_artificial)?,
            embedded: child.attr(gimli::DwAt(0x2903)).is_some_and(|attribute| {
                match attribute.value() {
                    gimli::AttributeValue::Flag(value) => value,
                    _ => attribute.udata_value().is_some_and(|value| value != 0),
                }
            }),
            declaration: None,
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "standard variant normalization validates the full nested DIE contract together"
    )]
    fn build_variant_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
        storage: VariantStorageKind,
    ) -> TypeEntry {
        let incomplete = match strict_flag(entry, gimli::DW_AT_declaration) {
            Ok(value) => value,
            Err(reason) => return TypeEntry::Malformed(reason),
        };
        if !incomplete && explicit_size.is_none() {
            return TypeEntry::Malformed("complete variant aggregate has no byte size".into());
        }
        let name = explicit_name
            .unwrap_or_else(|| Arc::from(format!("<anonymous variant@0x{:x}>", entry.offset().0)));
        let mut metadata_budget = VariantMetadataBudget::default();
        let record_kind = if storage == VariantStorageKind::Class {
            RecordKind::Class
        } else {
            RecordKind::Struct
        };
        let Some(unit) = self.units.get(unit_index) else {
            return TypeEntry::Malformed("variant aggregate unit is unavailable".into());
        };
        let mut tree = match unit.entries_tree(Some(entry.offset())) {
            Ok(tree) => tree,
            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
        };
        let root = match tree.root() {
            Ok(root) => root,
            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
        };
        let mut common_members = Vec::new();
        let mut bases = Vec::new();
        let mut discriminant = None;
        let mut variants = Vec::new();
        let mut children = root.children();
        loop {
            let child_node = match children.next() {
                Ok(Some(child)) => child,
                Ok(None) => break,
                Err(error) => return TypeEntry::Malformed(error.to_string().into()),
            };
            let child = child_node.entry();
            match child.tag() {
                gimli::DW_TAG_member => {
                    if metadata_budget.consume().is_err() {
                        return variant_metadata_limit_type(reference, &name, explicit_size);
                    }
                    let index = common_members.len();
                    let absent_offset = (storage == VariantStorageKind::Union).then_some(0_u64);
                    let member = match self.build_variant_member(
                        child,
                        unit,
                        unit_index,
                        reference.id,
                        DynamicAggregateChild::Member(index),
                        absent_offset,
                        record_kind,
                        AggregateMemberPath::Direct(index),
                    ) {
                        Ok(member) => member,
                        Err(reason) => return TypeEntry::Malformed(reason),
                    };
                    common_members.push(member);
                }
                gimli::DW_TAG_inheritance if storage != VariantStorageKind::Union => {
                    if metadata_budget.consume().is_err() {
                        return variant_metadata_limit_type(reference, &name, explicit_size);
                    }
                    let target = match self.target(child, unit_index) {
                        Ok(Some(target)) => target,
                        Ok(None) => {
                            return TypeEntry::Malformed("variant base class has no type".into());
                        }
                        Err(reason) => return TypeEntry::Malformed(reason),
                    };
                    let layout = Self::record_byte_layout(child);
                    if layout == RecordMemberLayout::Runtime {
                        match self.copy_dynamic_record_layout(child, unit_index) {
                            Ok(Some(expression)) => {
                                self.dynamic_record_layouts.insert(
                                    DynamicAggregateLayoutKey {
                                        aggregate: reference.id,
                                        child: DynamicAggregateChild::Base(bases.len()),
                                    },
                                    expression,
                                );
                            }
                            Ok(None) => {}
                            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
                        }
                    }
                    bases.push(BaseClass {
                        type_ref: target,
                        layout,
                        accessibility: match Self::record_accessibility(child, record_kind) {
                            Ok(accessibility) => accessibility,
                            Err(reason) => return TypeEntry::Malformed(reason),
                        },
                        virtuality: BaseClassVirtuality::None,
                    });
                }
                gimli::DW_TAG_variant_part => {
                    if discriminant.is_some() || !variants.is_empty() {
                        return TypeEntry::Malformed(
                            "variant aggregate contains multiple variant parts".into(),
                        );
                    }
                    let discr_key = match die_reference_with_signatures(
                        child.attr_value(gimli::DW_AT_discr),
                        unit_index,
                        self.units,
                        self.type_signatures,
                    ) {
                        Ok(key) => key,
                        Err(error) => return TypeEntry::Malformed(error.to_string().into()),
                    };
                    let tag_type = match self.target(child, unit_index) {
                        Ok(tag_type) => tag_type,
                        Err(reason) => return TypeEntry::Malformed(reason),
                    };
                    let variant_part_offset = child.offset();
                    let mut part_children = child_node.children();
                    if let Some(discr_key) = discr_key {
                        let mut stored = None;
                        loop {
                            let part_child = match part_children.next() {
                                Ok(Some(part_child)) => part_child,
                                Ok(None) => break,
                                Err(error) => {
                                    return TypeEntry::Malformed(error.to_string().into());
                                }
                            };
                            let part_child = part_child.entry();
                            if part_child.offset().0 != discr_key.offset
                                || discr_key.unit != unit_index
                            {
                                continue;
                            }
                            if part_child.tag() != gimli::DW_TAG_member {
                                return TypeEntry::Malformed(
                                    "DW_AT_discr does not reference a member child".into(),
                                );
                            }
                            if metadata_budget.consume().is_err() {
                                return variant_metadata_limit_type(
                                    reference,
                                    &name,
                                    explicit_size,
                                );
                            }
                            let member = match self.build_variant_member(
                                part_child,
                                unit,
                                unit_index,
                                reference.id,
                                DynamicAggregateChild::Discriminant,
                                None,
                                record_kind,
                                AggregateMemberPath::Discriminant,
                            ) {
                                Ok(member) => member,
                                Err(reason) => return TypeEntry::Malformed(reason),
                            };
                            stored = Some(member);
                        }
                        let Some(stored) = stored else {
                            return TypeEntry::Malformed(
                                "DW_AT_discr references a non-child discriminator".into(),
                            );
                        };
                        if let Some(tag_type) = tag_type
                            && tag_type.id != stored.type_ref.id
                        {
                            return TypeEntry::Malformed(
                                "variant tag type differs from its discriminator member".into(),
                            );
                        }
                        discriminant = Some(VariantDiscriminant::Stored(stored));
                    } else {
                        let Some(tag_type) = tag_type else {
                            return TypeEntry::Malformed(
                                "variant part has neither a discriminator nor a tag type".into(),
                            );
                        };
                        discriminant = Some(VariantDiscriminant::TagType(tag_type));
                    }
                    let representation = match discriminant.as_ref().expect("set above") {
                        VariantDiscriminant::Stored(member) => {
                            match self.resolved_integer_base(member.type_ref.id) {
                                Ok(base) => base,
                                Err(reason) => return TypeEntry::Malformed(reason),
                            }
                        }
                        VariantDiscriminant::TagType(tag_type) => {
                            match self.resolved_integer_base(tag_type.id) {
                                Ok(base) => base,
                                Err(reason) => return TypeEntry::Malformed(reason),
                            }
                        }
                    };

                    let mut variant_part_tree = match unit.entries_tree(Some(variant_part_offset)) {
                        Ok(tree) => tree,
                        Err(error) => return TypeEntry::Malformed(error.to_string().into()),
                    };
                    let variant_part_root = match variant_part_tree.root() {
                        Ok(root) => root,
                        Err(error) => return TypeEntry::Malformed(error.to_string().into()),
                    };
                    let mut part_children = variant_part_root.children();
                    loop {
                        let variant_node = match part_children.next() {
                            Ok(Some(part_child)) => part_child,
                            Ok(None) => break,
                            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
                        };
                        let variant_entry = variant_node.entry();
                        if variant_entry.tag() == gimli::DW_TAG_member {
                            continue;
                        }
                        if variant_entry.tag() != gimli::DW_TAG_variant {
                            return TypeEntry::Malformed(
                                format!(
                                    "variant part contains unsupported direct child {:?}",
                                    variant_entry.tag()
                                )
                                .into(),
                            );
                        }
                        if metadata_budget.consume().is_err() {
                            return variant_metadata_limit_type(reference, &name, explicit_size);
                        }
                        let selection = match copy_variant_selection(
                            variant_entry,
                            &representation,
                            self.byte_order,
                            &mut metadata_budget,
                        ) {
                            Ok(selection) => selection,
                            Err(VariantMetadataError::Malformed(reason)) => {
                                return TypeEntry::Malformed(reason);
                            }
                            Err(VariantMetadataError::Limit) => {
                                return variant_metadata_limit_type(
                                    reference,
                                    &name,
                                    explicit_size,
                                );
                            }
                        };
                        let variant_name = match copy_name(self.dwarf, unit, variant_entry) {
                            Ok(name) => name,
                            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
                        };
                        let variant_index = variants.len();
                        let mut members = Vec::new();
                        let mut variant_children = variant_node.children();
                        loop {
                            let member_node = match variant_children.next() {
                                Ok(Some(member)) => member,
                                Ok(None) => break,
                                Err(error) => {
                                    return TypeEntry::Malformed(error.to_string().into());
                                }
                            };
                            let member_entry = member_node.entry();
                            if member_entry.tag() != gimli::DW_TAG_member {
                                return TypeEntry::Malformed(
                                    format!(
                                        "variant contains unsupported component {:?}",
                                        member_entry.tag()
                                    )
                                    .into(),
                                );
                            }
                            if metadata_budget.consume().is_err() {
                                return variant_metadata_limit_type(
                                    reference,
                                    &name,
                                    explicit_size,
                                );
                            }
                            let member_index = members.len();
                            let member = match self.build_variant_member(
                                member_entry,
                                unit,
                                unit_index,
                                reference.id,
                                DynamicAggregateChild::VariantMember {
                                    variant: variant_index,
                                    member: member_index,
                                },
                                None,
                                record_kind,
                                AggregateMemberPath::Variant {
                                    variant: variant_index,
                                    member: member_index,
                                },
                            ) {
                                Ok(member) => member,
                                Err(reason) => return TypeEntry::Malformed(reason),
                            };
                            members.push(member);
                        }
                        variants.push(Variant {
                            name: variant_name,
                            selection,
                            members: members.into(),
                        });
                    }
                }
                gimli::DW_TAG_subprogram
                | gimli::DW_TAG_variable
                | gimli::DW_TAG_typedef
                | gimli::DW_TAG_structure_type
                | gimli::DW_TAG_class_type
                | gimli::DW_TAG_union_type
                | gimli::DW_TAG_enumeration_type
                | gimli::DW_TAG_template_type_parameter
                | gimli::DW_TAG_template_value_parameter => {}
                tag => {
                    return TypeEntry::Resolved(TypeInfo {
                        reference,
                        name,
                        byte_size: explicit_size,
                        kind: TypeKind::Opaque {
                            description: format!(
                                "variant aggregate contains unsupported direct child {tag:?}"
                            )
                            .into(),
                        },
                    });
                }
            }
        }
        let Some(discriminant) = discriminant else {
            return TypeEntry::Malformed("variant aggregate has no variant part".into());
        };
        if variants.is_empty() {
            return TypeEntry::Malformed("variant part has no variants".into());
        }
        if let Err(reason) = validate_variant_selections(&variants) {
            return TypeEntry::Malformed(reason);
        }
        TypeEntry::Resolved(TypeInfo {
            reference,
            name,
            byte_size: explicit_size,
            kind: TypeKind::Variant {
                storage,
                common_members: common_members.into(),
                bases: bases.into(),
                discriminant: Box::new(discriminant),
                variants: variants.into(),
                incomplete,
            },
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "record normalization keeps all storage and scope child tags in one auditable dispatch"
    )]
    fn build_record_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
    ) -> TypeEntry {
        let kind = if entry.tag() == gimli::DW_TAG_class_type {
            RecordKind::Class
        } else {
            RecordKind::Struct
        };
        let incomplete = match strict_flag(entry, gimli::DW_AT_declaration) {
            Ok(value) => value,
            Err(reason) => return TypeEntry::Malformed(reason),
        };
        if !incomplete && explicit_size.is_none() {
            return TypeEntry::Malformed("complete record type has no byte size".into());
        }
        match self.has_direct_variant_part(entry, unit_index) {
            Ok(true) => {
                return self.build_variant_type(
                    entry,
                    unit_index,
                    reference,
                    explicit_name,
                    explicit_size,
                    if kind == RecordKind::Class {
                        VariantStorageKind::Class
                    } else {
                        VariantStorageKind::Struct
                    },
                );
            }
            Ok(false) => {}
            Err(reason) => return TypeEntry::Malformed(reason),
        }
        let Some(unit) = self.units.get(unit_index) else {
            return TypeEntry::Malformed("record type unit is unavailable".into());
        };
        let mut tree = match unit.entries_tree(Some(entry.offset())) {
            Ok(tree) => tree,
            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
        };
        let root = match tree.root() {
            Ok(root) => root,
            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
        };
        let mut members = Vec::new();
        let mut bases = Vec::new();
        let mut children = root.children();
        loop {
            let child = match children.next() {
                Ok(Some(child)) => child,
                Ok(None) => break,
                Err(error) => return TypeEntry::Malformed(error.to_string().into()),
            };
            let child = child.entry();
            match child.tag() {
                gimli::DW_TAG_member => {
                    if members.len().saturating_add(bases.len()) >= MAX_RECORD_CHILDREN {
                        return TypeEntry::Malformed("record child count exceeds its limit".into());
                    }
                    let chain = match origin_chain(self.units, unit_index, child) {
                        Ok(chain) => chain,
                        Err(error) => return TypeEntry::Malformed(error.to_string().into()),
                    };
                    let target = match self.target_with_origins(child, unit_index, &chain) {
                        Ok(Some(target)) => target,
                        Ok(None) => {
                            return TypeEntry::Malformed("record member has no type".into());
                        }
                        Err(reason) => return TypeEntry::Malformed(reason),
                    };
                    let name =
                        match copy_name_with_origins(self.dwarf, self.units, unit, child, &chain) {
                            Ok(name) => name,
                            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
                        };
                    let layout = match self.record_member_layout(child, target, None) {
                        Ok(layout) => layout,
                        Err(reason) => return TypeEntry::Malformed(reason),
                    };
                    if layout == RecordMemberLayout::Runtime {
                        match self.copy_dynamic_record_layout(child, unit_index) {
                            Ok(Some(expression)) => {
                                self.dynamic_record_layouts.insert(
                                    DynamicAggregateLayoutKey {
                                        aggregate: reference.id,
                                        child: DynamicAggregateChild::Member(members.len()),
                                    },
                                    expression,
                                );
                            }
                            Ok(None) => {}
                            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
                        }
                    }
                    let member_index = members.len();
                    members.push(RecordMember {
                        name,
                        type_ref: target,
                        layout,
                        accessibility: match Self::record_accessibility(child, kind) {
                            Ok(accessibility) => accessibility,
                            Err(reason) => return TypeEntry::Malformed(reason),
                        },
                        artificial: match strict_flag(child, gimli::DW_AT_artificial) {
                            Ok(value) => value,
                            Err(reason) => return TypeEntry::Malformed(reason),
                        },
                        embedded: child.attr(gimli::DwAt(0x2903)).is_some_and(|attribute| {
                            match attribute.value() {
                                gimli::AttributeValue::Flag(value) => value,
                                _ => attribute.udata_value().is_some_and(|value| value != 0),
                            }
                        }),
                        declaration: None,
                    });
                    self.record_member_declarations
                        .push(AggregateMemberDeclaration {
                            aggregate: reference.id,
                            member: AggregateMemberPath::Direct(member_index),
                            die: DieKey {
                                unit: unit_index,
                                offset: child.offset().0,
                            },
                        });
                }
                gimli::DW_TAG_inheritance => {
                    if members.len().saturating_add(bases.len()) >= MAX_RECORD_CHILDREN {
                        return TypeEntry::Malformed("record child count exceeds its limit".into());
                    }
                    let target = match self.target(child, unit_index) {
                        Ok(Some(target)) => target,
                        Ok(None) => return TypeEntry::Malformed("base class has no type".into()),
                        Err(reason) => return TypeEntry::Malformed(reason),
                    };
                    let layout = Self::record_byte_layout(child);
                    if layout == RecordMemberLayout::Runtime {
                        match self.copy_dynamic_record_layout(child, unit_index) {
                            Ok(Some(expression)) => {
                                self.dynamic_record_layouts.insert(
                                    DynamicAggregateLayoutKey {
                                        aggregate: reference.id,
                                        child: DynamicAggregateChild::Base(bases.len()),
                                    },
                                    expression,
                                );
                            }
                            Ok(None) => {}
                            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
                        }
                    }
                    let virtuality = match child.attr_value(gimli::DW_AT_virtuality) {
                        None
                        | Some(gimli::AttributeValue::Virtuality(gimli::DW_VIRTUALITY_none)) => {
                            BaseClassVirtuality::None
                        }
                        Some(gimli::AttributeValue::Virtuality(value))
                            if value == gimli::DW_VIRTUALITY_virtual
                                || value == gimli::DW_VIRTUALITY_pure_virtual =>
                        {
                            BaseClassVirtuality::Virtual
                        }
                        _ => {
                            return TypeEntry::Malformed(
                                "base-class virtuality has an invalid encoding".into(),
                            );
                        }
                    };
                    bases.push(BaseClass {
                        type_ref: target,
                        layout,
                        accessibility: match Self::record_accessibility(child, kind) {
                            Ok(accessibility) => accessibility,
                            Err(reason) => return TypeEntry::Malformed(reason),
                        },
                        virtuality,
                    });
                }
                gimli::DW_TAG_variant_part => {
                    let name = explicit_name.unwrap_or_else(|| {
                        Arc::from(format!("<anonymous {:?}@0x{:x}>", kind, entry.offset().0))
                    });
                    return TypeEntry::Resolved(TypeInfo {
                        reference,
                        name,
                        byte_size: explicit_size,
                        kind: TypeKind::Opaque {
                            description:
                                "record contains a discriminated variant part that is unsupported"
                                    .into(),
                        },
                    });
                }
                // These DIEs describe class scope, not bytes in an instance.
                gimli::DW_TAG_subprogram
                | gimli::DW_TAG_variable
                | gimli::DW_TAG_typedef
                | gimli::DW_TAG_structure_type
                | gimli::DW_TAG_class_type
                | gimli::DW_TAG_union_type
                | gimli::DW_TAG_enumeration_type
                | gimli::DW_TAG_template_type_parameter
                | gimli::DW_TAG_template_value_parameter => {}
                tag => {
                    let name = explicit_name.unwrap_or_else(|| {
                        Arc::from(format!("<anonymous {:?}@0x{:x}>", kind, entry.offset().0))
                    });
                    return TypeEntry::Resolved(TypeInfo {
                        reference,
                        name,
                        byte_size: explicit_size,
                        kind: TypeKind::Opaque {
                            description: format!(
                                "record contains unsupported direct child {tag:?}"
                            )
                            .into(),
                        },
                    });
                }
            }
        }
        let name = explicit_name.unwrap_or_else(|| {
            Arc::from(format!("<anonymous {:?}@0x{:x}>", kind, entry.offset().0))
        });
        if let Some(normalized) = self.normalize_zig_optional_or_error_union(
            unit_index,
            reference.id,
            &name,
            &members,
            incomplete,
        ) {
            return match normalized {
                Ok(kind) => TypeEntry::Resolved(TypeInfo {
                    reference,
                    name,
                    byte_size: explicit_size,
                    kind,
                }),
                Err(reason) => TypeEntry::Malformed(reason),
            };
        }
        if let Some(normalized) =
            self.normalize_zig_tagged_union(unit_index, reference.id, &members, incomplete)
        {
            return match normalized {
                Ok(kind) => TypeEntry::Resolved(TypeInfo {
                    reference,
                    name,
                    byte_size: explicit_size,
                    kind,
                }),
                Err(reason) => TypeEntry::Malformed(reason),
            };
        }
        TypeEntry::Resolved(TypeInfo {
            reference,
            name,
            byte_size: explicit_size,
            kind: TypeKind::Record {
                kind,
                members: members.into(),
                bases: bases.into(),
                incomplete,
            },
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "union normalization keeps overlapping storage and scope children explicit"
    )]
    fn build_union_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
    ) -> TypeEntry {
        let incomplete = match strict_flag(entry, gimli::DW_AT_declaration) {
            Ok(value) => value,
            Err(reason) => return TypeEntry::Malformed(reason),
        };
        if !incomplete && explicit_size.is_none() {
            return TypeEntry::Malformed("complete union type has no byte size".into());
        }
        match self.has_direct_variant_part(entry, unit_index) {
            Ok(true) => {
                return self.build_variant_type(
                    entry,
                    unit_index,
                    reference,
                    explicit_name,
                    explicit_size,
                    VariantStorageKind::Union,
                );
            }
            Ok(false) => {}
            Err(reason) => return TypeEntry::Malformed(reason),
        }
        let name = explicit_name
            .unwrap_or_else(|| Arc::from(format!("<anonymous union@0x{:x}>", entry.offset().0)));
        let Some(unit) = self.units.get(unit_index) else {
            return TypeEntry::Malformed("union type unit is unavailable".into());
        };
        let mut tree = match unit.entries_tree(Some(entry.offset())) {
            Ok(tree) => tree,
            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
        };
        let root = match tree.root() {
            Ok(root) => root,
            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
        };
        let mut members = Vec::new();
        let mut children = root.children();
        loop {
            let child = match children.next() {
                Ok(Some(child)) => child,
                Ok(None) => break,
                Err(error) => return TypeEntry::Malformed(error.to_string().into()),
            };
            let child = child.entry();
            match child.tag() {
                gimli::DW_TAG_member => {
                    if members.len() >= MAX_RECORD_CHILDREN {
                        return TypeEntry::Resolved(TypeInfo {
                            reference,
                            name,
                            byte_size: explicit_size,
                            kind: TypeKind::Opaque {
                                description: "union member count exceeds its resource limit".into(),
                            },
                        });
                    }
                    let chain = match origin_chain(self.units, unit_index, child) {
                        Ok(chain) => chain,
                        Err(error) => return TypeEntry::Malformed(error.to_string().into()),
                    };
                    let target = match self.target_with_origins(child, unit_index, &chain) {
                        Ok(Some(target)) => target,
                        Ok(None) => return TypeEntry::Malformed("union member has no type".into()),
                        Err(reason) => return TypeEntry::Malformed(reason),
                    };
                    let member_name =
                        match copy_name_with_origins(self.dwarf, self.units, unit, child, &chain) {
                            Ok(name) => name,
                            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
                        };
                    let layout = match self.record_member_layout(child, target, Some(0)) {
                        Ok(layout) => layout,
                        Err(reason) => return TypeEntry::Malformed(reason),
                    };
                    if layout == RecordMemberLayout::Runtime {
                        match self.copy_dynamic_record_layout(child, unit_index) {
                            Ok(Some(expression)) => {
                                self.dynamic_record_layouts.insert(
                                    DynamicAggregateLayoutKey {
                                        aggregate: reference.id,
                                        child: DynamicAggregateChild::Member(members.len()),
                                    },
                                    expression,
                                );
                            }
                            Ok(None) => {}
                            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
                        }
                    }
                    let member_index = members.len();
                    members.push(RecordMember {
                        name: member_name,
                        type_ref: target,
                        layout,
                        accessibility: match Self::record_accessibility(child, RecordKind::Struct) {
                            Ok(accessibility) => accessibility,
                            Err(reason) => return TypeEntry::Malformed(reason),
                        },
                        artificial: match strict_flag(child, gimli::DW_AT_artificial) {
                            Ok(value) => value,
                            Err(reason) => return TypeEntry::Malformed(reason),
                        },
                        embedded: false,
                        declaration: None,
                    });
                    self.record_member_declarations
                        .push(AggregateMemberDeclaration {
                            aggregate: reference.id,
                            member: AggregateMemberPath::Direct(member_index),
                            die: DieKey {
                                unit: unit_index,
                                offset: child.offset().0,
                            },
                        });
                }
                gimli::DW_TAG_variant_part => {
                    return TypeEntry::Resolved(TypeInfo {
                        reference,
                        name,
                        byte_size: explicit_size,
                        kind: TypeKind::Opaque {
                            description:
                                "union contains a discriminated variant part that is unsupported"
                                    .into(),
                        },
                    });
                }
                gimli::DW_TAG_subprogram
                | gimli::DW_TAG_variable
                | gimli::DW_TAG_typedef
                | gimli::DW_TAG_structure_type
                | gimli::DW_TAG_class_type
                | gimli::DW_TAG_union_type
                | gimli::DW_TAG_enumeration_type
                | gimli::DW_TAG_template_type_parameter
                | gimli::DW_TAG_template_value_parameter => {}
                tag => {
                    return TypeEntry::Resolved(TypeInfo {
                        reference,
                        name,
                        byte_size: explicit_size,
                        kind: TypeKind::Opaque {
                            description: format!("union contains unsupported direct child {tag:?}")
                                .into(),
                        },
                    });
                }
            }
        }
        TypeEntry::Resolved(TypeInfo {
            reference,
            name,
            byte_size: explicit_size,
            kind: TypeKind::Union {
                members: members.into(),
                incomplete,
            },
        })
    }

    fn record_member_layout(
        &self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        target: TypeReference,
        absent_byte_offset: Option<u64>,
    ) -> std::result::Result<RecordMemberLayout, Arc<str>> {
        let bit_size = entry
            .attr(gimli::DW_AT_bit_size)
            .and_then(gimli::Attribute::udata_value);
        if let Some(bit_size) = bit_size {
            if bit_size == 0 {
                return Err("record bit-field has zero width".into());
            }
            if let Some(bit_offset) = entry
                .attr(gimli::DW_AT_data_bit_offset)
                .and_then(gimli::Attribute::udata_value)
            {
                bit_offset
                    .checked_add(bit_size)
                    .ok_or_else(|| Arc::from("record bit-field range overflows"))?;
                return Ok(RecordMemberLayout::BitRange {
                    bit_offset,
                    bit_size,
                });
            }
            if let Some(legacy_offset) = entry
                .attr(gimli::DW_AT_bit_offset)
                .and_then(gimli::Attribute::udata_value)
            {
                let byte_offset = entry
                    .attr(gimli::DW_AT_data_member_location)
                    .and_then(gimli::Attribute::udata_value)
                    .or(absent_byte_offset)
                    .ok_or_else(|| Arc::from("record member location is not a constant"))?;
                let storage_bytes = entry
                    .attr(gimli::DW_AT_byte_size)
                    .and_then(gimli::Attribute::udata_value)
                    .or_else(|| {
                        self.entries
                            .get(usize::try_from(target.id.get()).ok()?)
                            .and_then(|entry| match entry {
                                TypeEntry::Resolved(info) => info.byte_size,
                                TypeEntry::Building | TypeEntry::Malformed(_) => None,
                            })
                    })
                    .ok_or_else(|| Arc::from("legacy bit-field has no storage size"))?;
                let storage_bits = storage_bytes
                    .checked_mul(8)
                    .ok_or_else(|| Arc::from("legacy bit-field storage size overflows"))?;
                let within = match self.byte_order {
                    ByteOrder::Big => legacy_offset,
                    ByteOrder::Little => storage_bits
                        .checked_sub(legacy_offset)
                        .and_then(|value| value.checked_sub(bit_size))
                        .ok_or_else(|| Arc::from("legacy bit-field range exceeds storage"))?,
                };
                let bit_offset = byte_offset
                    .checked_mul(8)
                    .and_then(|value| value.checked_add(within))
                    .ok_or_else(|| Arc::from("legacy bit-field range overflows"))?;
                return Ok(RecordMemberLayout::BitRange {
                    bit_offset,
                    bit_size,
                });
            }
            return Err("bit-field has no bit offset".into());
        }
        Ok(entry.attr(gimli::DW_AT_data_member_location).map_or_else(
            || {
                absent_byte_offset
                    .map_or(RecordMemberLayout::Runtime, RecordMemberLayout::ByteOffset)
            },
            |attribute| {
                attribute
                    .udata_value()
                    .map_or(RecordMemberLayout::Runtime, RecordMemberLayout::ByteOffset)
            },
        ))
    }

    fn copy_dynamic_record_layout(
        &self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> std::result::Result<Option<Expression>, DwarfError> {
        let Some(gimli::AttributeValue::Exprloc(expression)) =
            entry.attr_value(gimli::DW_AT_data_member_location)
        else {
            return Ok(None);
        };
        let unit = self
            .units
            .get(unit_index)
            .ok_or(DwarfError::ReferenceOutsideUnits(unit_index))?;
        copy_expression(self.dwarf, unit_index, unit, expression, unit.encoding()).map(Some)
    }

    fn build_array_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
    ) -> TypeEntry {
        let element = match self.target(entry, unit_index) {
            Ok(Some(target)) => target,
            Ok(None) => return TypeEntry::Malformed("array type has no element type".into()),
            Err(reason) => return TypeEntry::Malformed(reason),
        };
        let Some(unit) = self.units.get(unit_index) else {
            return TypeEntry::Malformed("array type unit is unavailable".into());
        };
        let mut tree = match unit.entries_tree(Some(entry.offset())) {
            Ok(tree) => tree,
            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
        };
        let root = match tree.root() {
            Ok(root) => root,
            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
        };
        let mut dimensions = Vec::new();
        let mut children = root.children();
        while let Ok(Some(child)) = children.next() {
            if child.entry().tag() != gimli::DW_TAG_subrange_type {
                continue;
            }
            let child = child.entry();
            let lower = child
                .attr(gimli::DW_AT_lower_bound)
                .and_then(|a| {
                    a.sdata_value()
                        .map(i128::from)
                        .or_else(|| a.udata_value().map(i128::from))
                })
                .unwrap_or(0);
            let count = child
                .attr(gimli::DW_AT_count)
                .and_then(gimli::Attribute::udata_value)
                .or_else(|| {
                    child
                        .attr(gimli::DW_AT_upper_bound)
                        .and_then(|a| {
                            a.sdata_value()
                                .map(i128::from)
                                .or_else(|| a.udata_value().map(i128::from))
                        })
                        .and_then(|upper| {
                            u64::try_from(upper.checked_sub(lower)?.checked_add(1)?).ok()
                        })
                });
            let Some(count) = count else {
                return TypeEntry::Resolved(TypeInfo {
                    reference,
                    name: explicit_name.unwrap_or_else(|| Arc::from("<dynamic array>")),
                    byte_size: explicit_size,
                    kind: TypeKind::Opaque {
                        description: "array bounds are dynamic or missing".into(),
                    },
                });
            };
            dimensions.push(ArrayDimension {
                lower_bound: lower,
                count,
            });
        }
        if dimensions.is_empty() {
            return TypeEntry::Malformed("array type has no subrange dimensions".into());
        }
        let name =
            explicit_name.unwrap_or_else(|| Arc::from(format!("{}[]", self.target_name(element))));
        TypeEntry::Resolved(TypeInfo {
            reference,
            name,
            byte_size: explicit_size,
            kind: TypeKind::Array {
                element,
                dimensions: dimensions.into(),
            },
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "slice normalization validates both compiler layouts and every field invariant"
    )]
    fn build_slice_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
    ) -> TypeEntry {
        let name = explicit_name.expect("slice recognition requires a name");
        let Some(byte_size) = explicit_size else {
            return TypeEntry::Malformed("slice descriptor has no byte size".into());
        };
        let Some(unit) = self.units.get(unit_index) else {
            return TypeEntry::Malformed("slice type unit is unavailable".into());
        };
        let mut tree = match unit.entries_tree(Some(entry.offset())) {
            Ok(tree) => tree,
            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
        };
        let root = match tree.root() {
            Ok(root) => root,
            Err(error) => return TypeEntry::Malformed(error.to_string().into()),
        };
        let rust = name.starts_with("&[");
        let address_size = u64::from(unit.encoding().address_size);
        let zig = name.starts_with("[]") && byte_size == address_size.saturating_mul(2);
        let field_names = if rust {
            &["data_ptr", "length"][..]
        } else if zig {
            &["ptr", "len"][..]
        } else {
            &["array", "len", "cap"][..]
        };
        let field_count = u64::try_from(field_names.len()).expect("slice field count fits u64");
        let Some(word_size) = byte_size.checked_div(field_count) else {
            return TypeEntry::Malformed("slice descriptor size is invalid".into());
        };
        if byte_size != word_size * field_count || word_size != address_size {
            return TypeEntry::Resolved(TypeInfo {
                reference,
                name,
                byte_size: Some(byte_size),
                kind: TypeKind::Opaque {
                    description: "slice descriptor does not use target-sized words".into(),
                },
            });
        }
        let mut fields = Vec::new();
        let mut children = root.children();
        loop {
            let child = match children.next() {
                Ok(Some(child)) => child,
                Ok(None) => break,
                Err(error) => return TypeEntry::Malformed(error.to_string().into()),
            };
            if child.entry().tag() != gimli::DW_TAG_member {
                continue;
            }
            let child = child.entry();
            let field_name = match copy_name(self.dwarf, unit, child) {
                Ok(Some(name)) => name,
                Ok(None) => return TypeEntry::Malformed("slice member has no name".into()),
                Err(error) => return TypeEntry::Malformed(error.to_string().into()),
            };
            let Some(offset) = child
                .attr(gimli::DW_AT_data_member_location)
                .and_then(gimli::Attribute::udata_value)
            else {
                return TypeEntry::Malformed("slice member has no constant offset".into());
            };
            let field_type = match self.target(child, unit_index) {
                Ok(Some(target)) => target,
                Ok(None) => return TypeEntry::Malformed("slice member has no type".into()),
                Err(reason) => return TypeEntry::Malformed(reason),
            };
            fields.push((field_name, offset, field_type));
        }
        if fields.len() != field_names.len()
            || fields.iter().zip(field_names).enumerate().any(
                |(index, ((name, offset, _), expected_name))| {
                    name.as_ref() != *expected_name
                        || *offset
                            != u64::try_from(index).expect("field index fits u64") * word_size
                },
            )
        {
            return TypeEntry::Resolved(TypeInfo {
                reference,
                name,
                byte_size: Some(byte_size),
                kind: TypeKind::Opaque {
                    description: "unrecognized slice descriptor layout".into(),
                },
            });
        }
        let pointer = fields[0].2;
        let element = match self
            .entries
            .get(usize::try_from(pointer.id.get()).expect("type ID fits usize"))
        {
            Some(TypeEntry::Resolved(TypeInfo {
                kind:
                    TypeKind::Pointer {
                        target: Some(target),
                        ..
                    },
                ..
            })) => *target,
            _ => return TypeEntry::Malformed("slice data member is not a typed pointer".into()),
        };
        for (_, _, field_type) in &fields[1..] {
            let valid = self
                .entries
                .get(usize::try_from(field_type.id.get()).expect("type ID fits usize"))
                .is_some_and(|entry| {
                    matches!(entry, TypeEntry::Resolved(TypeInfo {
                        kind: TypeKind::Base(BaseType {
                            encoding: BaseTypeEncoding::Unsigned | BaseTypeEncoding::Signed,
                            byte_size: size,
                            ..
                        }),
                        ..
                    }) if *size == word_size)
                });
            if !valid {
                return TypeEntry::Malformed(
                    "slice length and capacity members must be target-sized unsigned integers"
                        .into(),
                );
            }
        }
        TypeEntry::Resolved(TypeInfo {
            reference,
            name,
            byte_size: Some(byte_size),
            kind: TypeKind::Slice {
                element,
                has_capacity: !rust && !zig,
            },
        })
    }
}

fn is_slice_type_name(name: &str) -> bool {
    name.starts_with("&[") || name.starts_with("[]")
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

    fn inspect_path(
        &self,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        root: &str,
        members: &[String],
        explicit_dereferences: u32,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
    ) -> Result<InspectedValue> {
        let object = self.visible_object(address, selected, root)?;
        if members.is_empty() && explicit_dereferences == 0 {
            let mut frame_base = FrameBaseCache::Empty;
            let variable =
                self.inspect_data_object(object, Some(address), context, runtime, &mut frame_base);
            return Ok(InspectedValue {
                type_info: variable.type_info,
                state: variable.state,
            });
        }
        let root_type = match &object.type_info {
            TypeResolution::Resolved(id) => *id,
            TypeResolution::Malformed(description) => {
                return Ok(InspectedValue {
                    type_info: None,
                    state: VariableState::Malformed(VariableMalformedReason {
                        description: Arc::clone(description),
                    }),
                });
            }
        };
        let plan = self.plan_path(root_type, members, explicit_dereferences)?;
        Ok(self.evaluate_path(object, plan, Some(address), context, runtime))
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

    fn inspect_global_path(
        &self,
        id: GlobalVariableId,
        address: Option<ImageAddress>,
        members: &[String],
        explicit_dereferences: u32,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
    ) -> Result<InspectedValue> {
        let global_index = usize::try_from(id.get()).expect("u32 fits usize");
        let object_index = *self
            .globals
            .get(global_index)
            .ok_or_else(|| Error::VariableNotFound(id.to_string()))?;
        let object = &self.objects[object_index];
        if members.is_empty() && explicit_dereferences == 0 {
            let mut frame_base = FrameBaseCache::Empty;
            let variable =
                self.inspect_data_object(object, address, context, runtime, &mut frame_base);
            return Ok(InspectedValue {
                type_info: variable.type_info,
                state: variable.state,
            });
        }
        let root_type = match &object.type_info {
            TypeResolution::Resolved(id) => *id,
            TypeResolution::Malformed(description) => {
                return Ok(InspectedValue {
                    type_info: None,
                    state: VariableState::Malformed(VariableMalformedReason {
                        description: Arc::clone(description),
                    }),
                });
            }
        };
        let plan = self.plan_path(root_type, members, explicit_dereferences)?;
        Ok(self.evaluate_path(object, plan, address, context, runtime))
    }

    fn dereference(
        &self,
        reference: &DereferenceReference,
        runtime: &mut dyn VariableRuntime,
    ) -> Result<DereferencedValue> {
        self.dereference_value(reference, runtime)
    }
}

/// The classified form of a `DW_AT_byte_size` attribute.
enum ByteSize {
    /// The attribute is not present; a default size may apply.
    Absent,
    /// A constant unsigned size in bytes.
    Constant(u64),
    /// A valid constant size the backend cannot represent (a `u128` above
    /// `u64::MAX`). Valid metadata, but not usable here.
    Unsupported(Arc<str>),
    /// A valid dynamic size (a location expression or DIE reference) that this
    /// backend cannot evaluate to a static width.
    Dynamic,
    /// A present attribute whose form is neither a constant nor a supported
    /// dynamic size, i.e. defective metadata.
    Malformed,
}

/// The classified form of an attribute expected to hold an unsigned integer
/// constant.
enum UnsignedConstant {
    /// A representable constant value.
    Value(u64),
    /// A valid constant that exceeds the representable `u64` range (a
    /// `DW_FORM_data16` value above `u64::MAX`). Valid metadata, unusable here.
    Oversized,
    /// A present attribute whose form is not an integer constant, i.e. defective.
    NonConstant,
}

/// Classifies an attribute expected to be an unsigned integer constant.
///
/// `udata_value` decodes the small constant forms but not the DWARF 5
/// `DW_FORM_data16`, so that form is handled explicitly. Every attribute that
/// must be an integer (byte size, address class, encoding) shares this so the
/// constant-versus-oversized-versus-defective distinction is made once.
fn unsigned_constant(attribute: &gimli::Attribute<Reader<'_>>) -> UnsignedConstant {
    if let Some(value) = attribute.udata_value() {
        return UnsignedConstant::Value(value);
    }
    match attribute.value() {
        gimli::AttributeValue::Data16(value) => {
            u64::try_from(value).map_or(UnsignedConstant::Oversized, UnsignedConstant::Value)
        }
        _ => UnsignedConstant::NonConstant,
    }
}

/// Resolves a type DIE's `DW_AT_address_class`.
///
/// An absent attribute defaults to zero. A present oversized constant is valid
/// but uninterpretable here (opaque); any other non-constant form is defective.
/// Silently treating either as the default class could produce a convincing read
/// using semantics the producer never specified.
fn resolve_address_class(
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    reference: TypeReference,
    explicit_name: Option<Arc<str>>,
) -> std::result::Result<u64, Box<TypeEntry>> {
    let Some(attribute) = entry.attr(gimli::DW_AT_address_class) else {
        return Ok(0);
    };
    match unsigned_constant(attribute) {
        UnsignedConstant::Value(value) => Ok(value),
        UnsignedConstant::Oversized => Err(Box::new(TypeEntry::Resolved(TypeInfo {
            reference,
            name: explicit_name.unwrap_or_else(|| Arc::from("<unsupported type>")),
            byte_size: None,
            kind: TypeKind::Opaque {
                description: "DW_AT_address_class exceeds the supported u64 range".into(),
            },
        }))),
        UnsignedConstant::NonConstant => Err(Box::new(TypeEntry::Malformed(
            "DW_AT_address_class is not an unsigned integer constant".into(),
        ))),
    }
}

/// Resolves the explicit `DW_AT_byte_size` for a type DIE.
///
/// Returns `Ok(Some(size))` for a usable constant, `Ok(None)` when the attribute
/// is absent (a default size may apply), or `Err(entry)` with the terminal
/// `TypeEntry` for a size that is valid-but-unusable or defective. Only a
/// genuinely absent attribute may fall back to a default; collapsing dynamic,
/// oversized, or malformed forms to "absent" would silently decode the wrong
/// width using semantics the producer never specified.
fn resolve_explicit_size(
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    reference: TypeReference,
    explicit_name: Option<Arc<str>>,
) -> std::result::Result<Option<u64>, Box<TypeEntry>> {
    match byte_size_attribute(entry) {
        ByteSize::Absent => Ok(None),
        ByteSize::Constant(size) => Ok(Some(size)),
        ByteSize::Unsupported(description) => Err(Box::new(TypeEntry::Resolved(TypeInfo {
            reference,
            name: explicit_name.unwrap_or_else(|| Arc::from("<oversized type>")),
            byte_size: None,
            kind: TypeKind::Opaque { description },
        }))),
        // A dynamic size is valid metadata this backend cannot statically size.
        // Mandatory tag attributes were already validated by the caller, so a
        // defect cannot be masked here.
        ByteSize::Dynamic => Err(Box::new(TypeEntry::Resolved(TypeInfo {
            reference,
            name: explicit_name.unwrap_or_else(|| Arc::from("<dynamically sized type>")),
            byte_size: None,
            kind: TypeKind::Opaque {
                description: "dynamic DW_AT_byte_size is unsupported".into(),
            },
        }))),
        ByteSize::Malformed => Err(Box::new(TypeEntry::Malformed(
            "DW_AT_byte_size is neither a constant nor a supported dynamic form".into(),
        ))),
    }
}

/// Whether a type DIE's `DW_AT_type` edge must be present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetRequirement {
    /// The tag mandates a target (e.g. a reference or qualifier).
    Required,
    /// The target is optional (e.g. a `void` pointer), but if present it must
    /// still name a real type DIE.
    Optional,
}

const fn named_type_target_requirement(declaration: bool) -> TargetRequirement {
    if declaration {
        TargetRequirement::Optional
    } else {
        TargetRequirement::Required
    }
}

fn named_type_relationship(
    tag: gimli::DwTag,
    language: Option<gimli::DwLang>,
    zig_producer: bool,
) -> NamedTypeRelationship {
    if tag == gimli::DW_TAG_template_alias {
        return NamedTypeRelationship::Synonym;
    }
    if zig_producer {
        return NamedTypeRelationship::Encoding;
    }
    match language {
        Some(
            gimli::DW_LANG_C89
            | gimli::DW_LANG_C
            | gimli::DW_LANG_C99
            | gimli::DW_LANG_C11
            | gimli::DW_LANG_C17
            | gimli::DW_LANG_C_plus_plus
            | gimli::DW_LANG_C_plus_plus_03
            | gimli::DW_LANG_C_plus_plus_11
            | gimli::DW_LANG_C_plus_plus_14
            | gimli::DW_LANG_C_plus_plus_17
            | gimli::DW_LANG_C_plus_plus_20,
        ) => NamedTypeRelationship::Synonym,
        Some(gimli::DW_LANG_Go) => NamedTypeRelationship::Distinct,
        Some(gimli::DW_LANG_Zig) => NamedTypeRelationship::Encoding,
        _ => NamedTypeRelationship::Unspecified,
    }
}

const fn type_modifier(tag: gimli::DwTag) -> Option<TypeModifier> {
    match tag {
        gimli::DW_TAG_const_type => Some(TypeModifier::Const),
        gimli::DW_TAG_volatile_type => Some(TypeModifier::Volatile),
        gimli::DW_TAG_restrict_type => Some(TypeModifier::Restrict),
        gimli::DW_TAG_atomic_type => Some(TypeModifier::Atomic),
        gimli::DW_TAG_immutable_type => Some(TypeModifier::Immutable),
        gimli::DW_TAG_packed_type => Some(TypeModifier::Packed),
        gimli::DW_TAG_shared_type => Some(TypeModifier::Shared),
        _ => None,
    }
}

/// Whether a DIE tag denotes a type. A `DW_AT_type` edge must name one of
/// these; a reference to any other tag is defective metadata. Tags this backend
/// does not model still count as types and are surfaced as opaque.
const fn is_type_die_tag(tag: gimli::DwTag) -> bool {
    matches!(
        tag,
        gimli::DW_TAG_base_type
            | gimli::DW_TAG_pointer_type
            | gimli::DW_TAG_reference_type
            | gimli::DW_TAG_rvalue_reference_type
            | gimli::DW_TAG_ptr_to_member_type
            | gimli::DW_TAG_array_type
            | gimli::DW_TAG_structure_type
            | gimli::DW_TAG_class_type
            | gimli::DW_TAG_union_type
            | gimli::DW_TAG_enumeration_type
            | gimli::DW_TAG_typedef
            | gimli::DW_TAG_template_alias
            | gimli::DW_TAG_const_type
            | gimli::DW_TAG_volatile_type
            | gimli::DW_TAG_restrict_type
            | gimli::DW_TAG_atomic_type
            | gimli::DW_TAG_immutable_type
            | gimli::DW_TAG_packed_type
            | gimli::DW_TAG_shared_type
            | gimli::DW_TAG_subroutine_type
            | gimli::DW_TAG_string_type
            | gimli::DW_TAG_set_type
            | gimli::DW_TAG_subrange_type
            | gimli::DW_TAG_file_type
            | gimli::DW_TAG_interface_type
            | gimli::DW_TAG_unspecified_type
            | gimli::DW_TAG_coarray_type
            | gimli::DW_TAG_dynamic_type
    )
}

/// Extracts a base type's `DW_AT_encoding` as a one-byte `DW_ATE_*` value.
///
/// The encoding domain is a single byte, so a present value outside `0..=255`
/// (or a non-constant form) is defective metadata rather than a vendor encoding
/// this backend merely does not implement.
fn base_type_encoding(
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
) -> std::result::Result<u8, Arc<str>> {
    let Some(attribute) = entry.attr(gimli::DW_AT_encoding) else {
        return Err("base type has no encoding".into());
    };
    match unsigned_constant(attribute) {
        UnsignedConstant::Value(value) => u8::try_from(value)
            .map_err(|_| Arc::from("DW_AT_encoding exceeds the one-byte DW_ATE domain")),
        UnsignedConstant::Oversized => {
            Err("DW_AT_encoding exceeds the one-byte DW_ATE domain".into())
        }
        UnsignedConstant::NonConstant => {
            Err("DW_AT_encoding is not an unsigned integer constant".into())
        }
    }
}

fn byte_size_attribute(entry: &gimli::DebuggingInformationEntry<Reader<'_>>) -> ByteSize {
    let Some(attribute) = entry.attr(gimli::DW_AT_byte_size) else {
        return ByteSize::Absent;
    };
    match unsigned_constant(attribute) {
        UnsignedConstant::Value(size) => ByteSize::Constant(size),
        UnsignedConstant::Oversized => {
            ByteSize::Unsupported("constant DW_AT_byte_size exceeds the supported u64 range".into())
        }
        // Not an integer constant. Per DWARF a byte size may instead be a
        // location expression or a reference to another DIE (class exprloc or
        // reference); those are valid but not statically sizable here. Every
        // other form is defective. `DW_FORM_sec_offset` (loclist/rnglist class)
        // is deliberately excluded: it is not a permitted `DW_AT_byte_size` form.
        UnsignedConstant::NonConstant => match attribute.value() {
            gimli::AttributeValue::Exprloc(_)
            | gimli::AttributeValue::Block(_)
            | gimli::AttributeValue::UnitRef(_)
            | gimli::AttributeValue::DebugInfoRef(_)
            | gimli::AttributeValue::DebugInfoRefSup(_)
            | gimli::AttributeValue::DebugTypesRef(_) => ByteSize::Dynamic,
            _ => ByteSize::Malformed,
        },
    }
}

trait TypeMetadataEntry {
    fn type_info(&self) -> std::result::Result<&TypeInfo, Arc<str>>;
}

impl TypeMetadataEntry for TypeEntry {
    fn type_info(&self) -> std::result::Result<&TypeInfo, Arc<str>> {
        match self {
            Self::Resolved(info) => Ok(info),
            Self::Malformed(reason) => Err(Arc::clone(reason)),
            Self::Building => Err("type graph did not finish building".into()),
        }
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

fn type_info_from<T: TypeMetadataEntry>(
    types: &[T],
    id: TypeId,
) -> std::result::Result<&TypeInfo, Arc<str>> {
    types
        .get(usize::try_from(id.get()).expect("type ID fits usize"))
        .map_or_else(
            || Err("type ID is outside the module arena".into()),
            TypeMetadataEntry::type_info,
        )
}

fn propagate_wrapper_sizes(types: &mut [TypeEntry]) {
    let mut dependents = vec![Vec::new(); types.len()];
    let mut ready = VecDeque::new();
    for (index, entry) in types.iter().enumerate() {
        let TypeEntry::Resolved(info) = entry else {
            continue;
        };
        if info.byte_size.is_some() {
            ready.push_back(index);
            continue;
        }
        let (TypeKind::Modified { target, .. }
        | TypeKind::Named {
            target: Some(target),
            ..
        }) = info.kind
        else {
            continue;
        };
        if let Ok(target) = usize::try_from(target.id.get())
            && let Some(target_dependents) = dependents.get_mut(target)
        {
            target_dependents.push(index);
        }
    }

    while let Some(target) = ready.pop_front() {
        let Some(size) = types
            .get(target)
            .and_then(|entry| entry.type_info().ok())
            .and_then(|info| info.byte_size)
        else {
            continue;
        };
        for dependent in std::mem::take(&mut dependents[target]) {
            let Some(TypeEntry::Resolved(info)) = types.get_mut(dependent) else {
                continue;
            };
            if info.byte_size.is_none() {
                info.byte_size = Some(size);
                ready.push_back(dependent);
            }
        }
    }
}

fn inline_storage_cycle_nodes(types: &[TypeEntry]) -> Vec<usize> {
    let mut edges = vec![Vec::new(); types.len()];
    for (index, entry) in types.iter().enumerate() {
        let TypeEntry::Resolved(info) = entry else {
            continue;
        };
        inline_storage_targets(&info.kind, &mut edges[index]);
        edges[index].retain(|target| *target < types.len());
    }
    let mut reverse = vec![Vec::new(); edges.len()];
    for (source, targets) in edges.iter().enumerate() {
        for target in targets {
            reverse[*target].push(source);
        }
    }

    let mut visited = vec![false; edges.len()];
    let mut finish = Vec::with_capacity(edges.len());
    for start in 0..edges.len() {
        if visited[start] {
            continue;
        }
        visited[start] = true;
        let mut stack = vec![(start, 0_usize)];
        while let Some((node, edge_index)) = stack.last_mut() {
            if let Some(next) = edges[*node].get(*edge_index).copied() {
                *edge_index += 1;
                if !visited[next] {
                    visited[next] = true;
                    stack.push((next, 0));
                }
            } else {
                finish.push(*node);
                stack.pop();
            }
        }
    }

    let mut assigned = vec![false; edges.len()];
    let mut cyclic_nodes = Vec::new();
    for start in finish.into_iter().rev() {
        if assigned[start] {
            continue;
        }
        assigned[start] = true;
        let mut component = Vec::new();
        let mut stack = vec![start];
        while let Some(node) = stack.pop() {
            component.push(node);
            for predecessor in &reverse[node] {
                if !assigned[*predecessor] {
                    assigned[*predecessor] = true;
                    stack.push(*predecessor);
                }
            }
        }
        let cyclic = component.len() > 1
            || component
                .first()
                .is_some_and(|node| edges[*node].contains(node));
        if cyclic {
            cyclic_nodes.extend(component);
        }
    }
    cyclic_nodes
}

fn inline_storage_targets(kind: &TypeKind, targets: &mut Vec<usize>) {
    let mut push = |reference: TypeReference| {
        if let Ok(index) = usize::try_from(reference.id.get()) {
            targets.push(index);
        }
    };
    match kind {
        TypeKind::Enumeration {
            underlying: Some(target),
            ..
        }
        | TypeKind::Modified { target, .. }
        | TypeKind::Named {
            target: Some(target),
            ..
        } => push(*target),
        TypeKind::Array { element, .. } => push(*element),
        TypeKind::Record { members, bases, .. } => {
            for member in members.iter() {
                push(member.type_ref);
            }
            for base in bases.iter() {
                push(base.type_ref);
            }
        }
        TypeKind::Union { members, .. } => {
            for member in members.iter() {
                push(member.type_ref);
            }
        }
        TypeKind::Variant {
            common_members,
            bases,
            discriminant,
            variants,
            ..
        } => {
            for member in common_members.iter() {
                push(member.type_ref);
            }
            for base in bases.iter() {
                push(base.type_ref);
            }
            if let VariantDiscriminant::Stored(member) = discriminant.as_ref() {
                push(member.type_ref);
            }
            for variant in variants.iter() {
                for member in variant.members.iter() {
                    push(member.type_ref);
                }
            }
        }
        TypeKind::Base(_)
        | TypeKind::Enumeration {
            underlying: None, ..
        }
        | TypeKind::Pointer { .. }
        | TypeKind::Reference { .. }
        | TypeKind::Slice { .. }
        | TypeKind::Named { target: None, .. }
        | TypeKind::Unspecified
        | TypeKind::Opaque { .. } => {}
    }
}

fn modifier_type_name(modifier: TypeModifier, target: &str, indirection: bool) -> String {
    let keyword = match modifier {
        TypeModifier::Const => "const",
        TypeModifier::Volatile => "volatile",
        TypeModifier::Restrict => "restrict",
        TypeModifier::Immutable => "immutable",
        TypeModifier::Packed => "packed",
        TypeModifier::Shared => "shared",
        TypeModifier::Atomic => return format!("_Atomic({target})"),
    };
    if indirection {
        format!("{target} {keyword}")
    } else {
        format!("{keyword} {target}")
    }
}

enum TransparentRepresentationError {
    Malformed(Arc<str>),
    Unsupported(Arc<str>),
}

fn transparent_representation<T: TypeMetadataEntry>(
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
enum ValueShapeError {
    /// The type graph is defective: a wrapper cycle, an indirection with no
    /// byte size, or an underlying malformed/incomplete type entry.
    Malformed(Arc<str>),
    /// The type is valid but its value shape is not implemented.
    Unsupported(Arc<str>),
}

/// The widest indirection representation `decode_address` can turn into a
/// `VirtualAddress`.
const MAX_ADDRESS_BYTES: u64 = 8;

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
fn indirection_byte_size(
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

#[expect(
    clippy::too_many_lines,
    reason = "each normalized type shape has distinct validation"
)]
fn value_shape_from<T: TypeMetadataEntry>(
    types: &[T],
    id: TypeId,
) -> std::result::Result<ValueShape, ValueShapeError> {
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
                    type_info: type_info_from(types, id)
                        .map_err(ValueShapeError::Malformed)?
                        .clone(),
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
                    type_info: type_info_from(types, id)
                        .map_err(ValueShapeError::Malformed)?
                        .clone(),
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
                let element_shape = value_shape_from(types, element.id)?;
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
                if count > 100_000 || byte_size > 1_024 * 1_024 {
                    return Err(ValueShapeError::Unsupported(
                        "array exceeds inspection limits".into(),
                    ));
                }
                return Ok(ValueShape {
                    type_info: type_info_from(types, id)
                        .map_err(ValueShapeError::Malformed)?
                        .clone(),
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
            } => {
                let byte_size = info.byte_size.ok_or_else(|| {
                    ValueShapeError::Malformed("slice descriptor has no byte size".into())
                })?;
                return Ok(ValueShape {
                    type_info: type_info_from(types, id)
                        .map_err(ValueShapeError::Malformed)?
                        .clone(),
                    kind: ValueShapeKind::Slice {
                        element: element.id,
                        byte_size,
                        has_capacity: *has_capacity,
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
                    type_info: type_info_from(types, id)
                        .map_err(ValueShapeError::Malformed)?
                        .clone(),
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
                    type_info: type_info_from(types, id)
                        .map_err(ValueShapeError::Malformed)?
                        .clone(),
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
                    && !matches!(
                        variants.as_ref(),
                        [Variant {
                            selection: VariantSelection::Default,
                            ..
                        }]
                    )
                {
                    return Err(ValueShapeError::Unsupported(
                        "tagless variant selection is unsupported".into(),
                    ));
                }
                let byte_size = info.byte_size.ok_or_else(|| {
                    ValueShapeError::Malformed("complete variant type has no byte size".into())
                })?;
                return Ok(ValueShape {
                    type_info: type_info_from(types, id)
                        .map_err(ValueShapeError::Malformed)?
                        .clone(),
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
                    type_info: type_info_from(types, id)
                        .map_err(ValueShapeError::Malformed)?
                        .clone(),
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
                    type_info: type_info_from(types, id)
                        .map_err(ValueShapeError::Malformed)?
                        .clone(),
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

impl DwarfVariableInfo {
    fn type_info(&self, id: TypeId) -> std::result::Result<&TypeInfo, Arc<str>> {
        type_info_from(&self.types, id)
    }

    fn value_shape(&self, id: TypeId) -> std::result::Result<ValueShape, ValueShapeError> {
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

    fn transparent_type(
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

    fn validate_static_member_layout(&self, record: TypeId, member: &RecordMember) -> Result<()> {
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

    #[expect(
        clippy::too_many_lines,
        reason = "path planning keeps type traversal and its typed failures in one auditable state machine"
    )]
    fn plan_path(
        &self,
        root: TypeId,
        members: &[String],
        explicit_dereferences: u32,
    ) -> Result<PlannedPath> {
        enum AggregateMembers<'a> {
            Direct(&'a [RecordMember]),
            Variant {
                common_members: &'a [RecordMember],
                discriminant: &'a VariantDiscriminant,
                variants: &'a Arc<[Variant]>,
            },
        }

        let mut current = root;
        let mut steps = Vec::new();
        for member_name in members {
            let mut indirections = HashSet::new();
            let (aggregate, aggregate_members) = loop {
                let source_info = self.type_info(current).map_err(|description| {
                    Error::debug_info(DwarfError::MalformedVariable(description))
                })?;
                let (canonical, info) = match self.transparent_type(current) {
                    Ok(value) => value,
                    Err(ValueShapeError::Malformed(description)) => {
                        return Err(Error::debug_info(DwarfError::MalformedVariable(
                            description,
                        )));
                    }
                    Err(ValueShapeError::Unsupported(description)) => {
                        steps.push(PathStep::Unavailable(VariableUnavailableReason::Other(
                            description,
                        )));
                        return Ok(PlannedPath {
                            steps,
                            terminal: None,
                        });
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
                        if !indirections.insert(canonical) || steps.len() >= MAX_AGGREGATE_DEPTH {
                            return Err(Error::InvalidValueExpression(
                                "pointer traversal exceeds its limit or contains a cycle"
                                    .to_owned(),
                            ));
                        }
                        let byte_size = match indirection_byte_size(
                            info.byte_size,
                            *address_class,
                            "pointer or reference",
                        ) {
                            Ok(byte_size) => byte_size,
                            Err(ValueShapeError::Malformed(description)) => {
                                return Err(Error::debug_info(DwarfError::MalformedVariable(
                                    description,
                                )));
                            }
                            Err(ValueShapeError::Unsupported(description)) => {
                                steps.push(PathStep::Unavailable(
                                    VariableUnavailableReason::Other(description),
                                ));
                                current = target.id;
                                continue;
                            }
                        };
                        steps.push(PathStep::Dereference {
                            target: target.id,
                            byte_size,
                            address_class: *address_class,
                        });
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
                            member: member_name.clone(),
                            type_name: Arc::clone(&source_info.name),
                        });
                    }
                }
            };
            let mut matching = Vec::new();
            match aggregate_members {
                AggregateMembers::Direct(members) => {
                    matching.extend(
                        members
                            .iter()
                            .enumerate()
                            .filter(|(_, member)| {
                                !member.artificial
                                    && member.name.as_deref() == Some(member_name.as_str())
                            })
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
                            .filter(|(_, member)| {
                                !member.artificial
                                    && member.name.as_deref() == Some(member_name.as_str())
                            })
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
                                .filter(|(_, member)| {
                                    !member.artificial
                                        && member.name.as_deref() == Some(member_name.as_str())
                                })
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
                        member: member_name.clone(),
                        type_name,
                    });
                }
                return Err(Error::AmbiguousMember {
                    member: member_name.clone(),
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
            current = member.type_ref.id;
        }
        for _ in 0..explicit_dereferences {
            let (_canonical, info) = match self.transparent_type(current) {
                Ok(value) => value,
                Err(ValueShapeError::Malformed(description)) => {
                    return Err(Error::debug_info(DwarfError::MalformedVariable(
                        description,
                    )));
                }
                Err(ValueShapeError::Unsupported(description)) => {
                    steps.push(PathStep::Unavailable(VariableUnavailableReason::Other(
                        description,
                    )));
                    return Ok(PlannedPath {
                        steps,
                        terminal: None,
                    });
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
                    steps.push(PathStep::Unavailable(VariableUnavailableReason::Other(
                        DereferenceUnavailableReason::UnspecifiedPointee
                            .to_string()
                            .into(),
                    )));
                    return Ok(PlannedPath {
                        steps,
                        terminal: None,
                    });
                }
                _ => {
                    steps.push(PathStep::Unavailable(VariableUnavailableReason::Other(
                        "the value is not a pointer or reference".into(),
                    )));
                    return Ok(PlannedPath {
                        steps,
                        terminal: Some(current),
                    });
                }
            };
            let byte_size = match indirection_byte_size(
                info.byte_size,
                address_class,
                "pointer or reference",
            ) {
                Ok(byte_size) => byte_size,
                Err(ValueShapeError::Malformed(description)) => {
                    return Err(Error::debug_info(DwarfError::MalformedVariable(
                        description,
                    )));
                }
                Err(ValueShapeError::Unsupported(description)) => {
                    steps.push(PathStep::Unavailable(VariableUnavailableReason::Other(
                        description,
                    )));
                    current = target;
                    continue;
                }
            };
            steps.push(PathStep::Dereference {
                target,
                byte_size,
                address_class,
            });
            current = target;
        }
        Ok(PlannedPath {
            steps,
            terminal: Some(current),
        })
    }

    fn visible_object(
        &self,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        name: &str,
    ) -> Result<&CatalogDataObject> {
        let function = self
            .function_at(address)
            .ok_or_else(|| Error::VariableNotFound(name.to_owned()))?;
        let mut named = function
            .objects
            .iter()
            .map(|&index| &self.objects[index])
            .filter(|object| object.instance == selected)
            .filter(|object| object.ranges.iter().any(|range| range.contains(address)))
            .filter(|object| object.name.as_ref() == name)
            .collect::<Vec<_>>();
        let depth = named
            .iter()
            .map(|object| object.lexical_depth)
            .max()
            .ok_or_else(|| Error::VariableNotFound(name.to_owned()))?;
        named.retain(|object| object.lexical_depth == depth);
        let [object] = named.as_slice() else {
            return Err(Error::AmbiguousVariable(name.to_owned()));
        };
        Ok(*object)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "location selection preserves each DWARF storage form and its typed failure"
    )]
    fn located_data_object(
        &self,
        variable: &CatalogDataObject,
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        frame_base_cache: &mut FrameBaseCache,
        budget: &mut EvaluationBudget,
    ) -> std::result::Result<LocatedStorage, PathEvaluationError> {
        if let Some(description) = &variable.malformed {
            return Err(PathEvaluationError::Malformed(Arc::clone(description)));
        }
        let type_id = match &variable.type_info {
            TypeResolution::Resolved(id) => *id,
            TypeResolution::Malformed(description) => {
                return Err(PathEvaluationError::Malformed(Arc::clone(description)));
            }
        };
        let shape = self.value_shape(type_id).map_err(|error| match error {
            ValueShapeError::Malformed(description) => PathEvaluationError::Malformed(description),
            ValueShapeError::Unsupported(description) => {
                PathEvaluationError::Unavailable(VariableUnavailableReason::Other(description))
            }
        })?;
        let description = match &variable.value {
            Metadata::Value(description) => description,
            Metadata::Unavailable(description) => {
                let reason = if description.as_ref() == "no location was supplied" {
                    VariableUnavailableReason::OptimizedOut
                } else {
                    Arc::clone(description).into()
                };
                return Err(PathEvaluationError::Unavailable(reason));
            }
            Metadata::Malformed(description) => {
                return Err(PathEvaluationError::Malformed(Arc::clone(description)));
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
        let expression = location.expression(address)?.ok_or_else(|| {
            VariableUnavailableReason::Other("no location at the current instruction".into())
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
        )
        .map_err(|error| match error {
            EvaluateError::Unavailable(reason) => PathEvaluationError::Unavailable(reason),
            EvaluateError::Malformed(description) => PathEvaluationError::Malformed(description),
        })?;
        if pieces.len() > MAX_LOCATION_PIECES {
            return Err(VariableUnavailableReason::EvaluationLimit.into());
        }
        let [piece] = pieces.as_slice() else {
            return Err(crate::UnsupportedVariableFeature::CompositeLocation.into());
        };
        let expected_bits = shape
            .byte_size()
            .checked_mul(8)
            .ok_or(VariableUnavailableReason::EvaluationLimit)?;
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

    fn storage_with_offset(
        storage: LocatedStorage,
        offset: i64,
    ) -> std::result::Result<LocatedStorage, PathEvaluationError> {
        match storage {
            LocatedStorage::Memory(address) => {
                let value = if offset >= 0 {
                    address.get().checked_add(offset.unsigned_abs())
                } else {
                    address.get().checked_sub(offset.unsigned_abs())
                }
                .ok_or_else(|| {
                    PathEvaluationError::Unavailable(VariableUnavailableReason::Other(
                        "member address overflows".into(),
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
                    PathEvaluationError::Malformed(
                        "member offset is outside its containing value".into(),
                    )
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
            LocatedStorage::ImplicitPointer { .. } => Err(PathEvaluationError::Malformed(
                "an unresolved implicit pointer cannot be offset".into(),
            )),
        }
    }

    fn read_storage(
        storage: &LocatedStorage,
        size: usize,
        runtime: &mut dyn VariableRuntime,
        budget: &mut EvaluationBudget,
    ) -> std::result::Result<(VariableValueSource, Arc<[u8]>), PathEvaluationError> {
        match storage {
            LocatedStorage::Memory(address) => {
                budget.consume_memory(size)?;
                let raw = runtime
                    .read_memory(*address, size)
                    .map_err(VariableUnavailableReason::Other)?;
                Ok((VariableValueSource::Memory(*address), raw))
            }
            LocatedStorage::Bytes {
                source,
                raw,
                start,
                end,
                ..
            } => {
                let selected_end = start
                    .checked_add(size)
                    .filter(|selected_end| *selected_end <= *end)
                    .ok_or_else(|| {
                        PathEvaluationError::Malformed(
                            "selected value extends beyond its containing storage".into(),
                        )
                    })?;
                Ok((source.clone(), Arc::from(&raw[*start..selected_end])))
            }
            LocatedStorage::ImplicitPointer { .. } => Err(PathEvaluationError::Malformed(
                "an unresolved implicit pointer cannot be read".into(),
            )),
        }
    }

    const fn concrete_storage_address(storage: &LocatedStorage) -> Option<VirtualAddress> {
        match storage {
            LocatedStorage::Memory(address) => Some(*address),
            LocatedStorage::Bytes { address, .. } => *address,
            LocatedStorage::ImplicitPointer { .. } => None,
        }
    }

    fn bit_field_storage(
        &self,
        storage: LocatedStorage,
        type_id: TypeId,
        bit_offset: u64,
        bit_size: u64,
        runtime: &mut dyn VariableRuntime,
        budget: &mut EvaluationBudget,
    ) -> std::result::Result<LocatedStorage, PathEvaluationError> {
        let shape = self.value_shape(type_id).map_err(|error| match error {
            ValueShapeError::Malformed(description) => PathEvaluationError::Malformed(description),
            ValueShapeError::Unsupported(description) => {
                PathEvaluationError::Unavailable(VariableUnavailableReason::Other(description))
            }
        })?;
        let base = match &shape.kind {
            ValueShapeKind::Scalar(base) => base,
            ValueShapeKind::Enumeration { representation, .. } => representation,
            _ => {
                return Err(PathEvaluationError::Unavailable(
                    VariableUnavailableReason::Other(
                        "non-integral bit-fields are unsupported".into(),
                    ),
                ));
            }
        };
        let storage_bits = base
            .byte_size
            .checked_mul(8)
            .ok_or(VariableUnavailableReason::EvaluationLimit)?;
        if bit_size == 0 || bit_size > storage_bits || bit_size > 128 {
            return Err(PathEvaluationError::Malformed(
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
                .map_err(PathEvaluationError::Malformed)?;
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

    fn runtime_member_storage(
        &self,
        storage: &LocatedStorage,
        aggregate: TypeId,
        child: DynamicAggregateChild,
        runtime: &mut dyn VariableRuntime,
        budget: &mut EvaluationBudget,
    ) -> std::result::Result<LocatedStorage, PathEvaluationError> {
        let object_address = Self::concrete_storage_address(storage).ok_or_else(|| {
            PathEvaluationError::Unavailable(VariableUnavailableReason::Other(
                "runtime member location has no concrete containing-object address".into(),
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

    fn active_variant_from_storage(
        &self,
        storage: &LocatedStorage,
        aggregate: TypeId,
        discriminant: &VariantDiscriminant,
        variants: &[Variant],
        runtime: &mut dyn VariableRuntime,
        budget: &mut EvaluationBudget,
    ) -> std::result::Result<Option<usize>, PathEvaluationError> {
        let VariantDiscriminant::Stored(member) = discriminant else {
            return Ok(Some(0));
        };
        let shape = self
            .value_shape(member.type_ref.id)
            .map_err(|error| match error {
                ValueShapeError::Malformed(description) => {
                    PathEvaluationError::Malformed(description)
                }
                ValueShapeError::Unsupported(description) => {
                    PathEvaluationError::Unavailable(VariableUnavailableReason::Other(description))
                }
            })?;
        let representation = match &shape.kind {
            ValueShapeKind::Scalar(base)
                if !matches!(base.encoding, BaseTypeEncoding::Floating) =>
            {
                base
            }
            ValueShapeKind::Enumeration { representation, .. } => representation,
            _ => {
                return Err(PathEvaluationError::Malformed(
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
            .map_err(PathEvaluationError::Malformed)?;
        selected_variant_index(variants, value).map_err(PathEvaluationError::Malformed)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "implicit-pointer resolution requires target bounds and the shared evaluation context"
    )]
    fn resolve_implicit_pointer(
        &self,
        debug_info_offset: u64,
        byte_offset: i64,
        target: TypeId,
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        frame_base_cache: &mut FrameBaseCache,
        budget: &mut EvaluationBudget,
    ) -> std::result::Result<LocatedStorage, PathEvaluationError> {
        let object_index = self
            .objects_by_debug_offset
            .get(&debug_info_offset)
            .copied()
            .ok_or_else(|| {
                PathEvaluationError::Unavailable(
                    crate::UnsupportedVariableFeature::CrossDieEvaluation.into(),
                )
            })?;
        let object = &self.objects[object_index];
        let referenced_type = match &object.type_info {
            TypeResolution::Resolved(id) => *id,
            TypeResolution::Malformed(description) => {
                return Err(PathEvaluationError::Malformed(Arc::clone(description)));
            }
        };
        let referenced_size = self
            .value_shape(referenced_type)
            .map_err(|error| match error {
                ValueShapeError::Malformed(description) => {
                    PathEvaluationError::Malformed(description)
                }
                ValueShapeError::Unsupported(description) => {
                    PathEvaluationError::Unavailable(VariableUnavailableReason::Other(description))
                }
            })?
            .byte_size();
        let target_size = self
            .value_shape(target)
            .map_err(|error| match error {
                ValueShapeError::Malformed(description) => {
                    PathEvaluationError::Malformed(description)
                }
                ValueShapeError::Unsupported(description) => {
                    PathEvaluationError::Unavailable(VariableUnavailableReason::Other(description))
                }
            })?
            .byte_size();
        implicit_pointer_range(byte_offset, target_size, referenced_size)
            .map_err(PathEvaluationError::Unavailable)?;
        let referenced =
            self.located_data_object(object, address, runtime, frame_base_cache, budget)?;
        if matches!(referenced, LocatedStorage::ImplicitPointer { .. }) {
            return Err(PathEvaluationError::Unavailable(
                crate::UnsupportedVariableFeature::CrossDieEvaluation.into(),
            ));
        }
        Self::storage_with_offset(referenced, byte_offset)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "path evaluation keeps ordered storage transitions and typed failures in one auditable state machine"
    )]
    fn evaluate_path(
        &self,
        variable: &CatalogDataObject,
        plan: PlannedPath,
        address: Option<ImageAddress>,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
    ) -> InspectedValue {
        let terminal_type = match plan.terminal {
            Some(terminal) => match self.type_info(terminal) {
                Ok(info) => Some(info.clone()),
                Err(description) => {
                    return InspectedValue {
                        type_info: None,
                        state: VariableState::Malformed(VariableMalformedReason { description }),
                    };
                }
            },
            None => None,
        };
        let failure = |error: PathEvaluationError| InspectedValue {
            type_info: terminal_type.clone(),
            state: match error {
                PathEvaluationError::Unavailable(reason) => VariableState::Unavailable(reason),
                PathEvaluationError::Malformed(description) => {
                    VariableState::Malformed(VariableMalformedReason { description })
                }
            },
        };
        let mut budget = EvaluationBudget::default();
        let mut frame_base = FrameBaseCache::Empty;
        let mut storage = match self.located_data_object(
            variable,
            address,
            runtime,
            &mut frame_base,
            &mut budget,
        ) {
            Ok(storage) => storage,
            Err(error) => return failure(error),
        };
        for step in plan.steps {
            storage = match step {
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
                        match self.resolve_implicit_pointer(
                            debug_info_offset,
                            byte_offset,
                            target,
                            address,
                            runtime,
                            &mut frame_base,
                            &mut budget,
                        ) {
                            Ok(storage) => storage,
                            Err(error) => return failure(error),
                        }
                    } else {
                        if address_class != 0 {
                            return failure(PathEvaluationError::Unavailable(
                                VariableUnavailableReason::Other(
                                    DereferenceUnavailableReason::AddressClass(address_class)
                                        .to_string()
                                        .into(),
                                ),
                            ));
                        }
                        let Ok(size) = usize::try_from(byte_size) else {
                            return failure(VariableUnavailableReason::EvaluationLimit.into());
                        };
                        let (_, raw) =
                            match Self::read_storage(&storage, size, runtime, &mut budget) {
                                Ok(value) => value,
                                Err(error) => return failure(error),
                            };
                        let pointer = match decode_address(&raw, byte_size, self.target) {
                            Ok(pointer) => pointer,
                            Err(reason) => return failure(reason.into()),
                        };
                        if pointer.get() == 0 {
                            return failure(PathEvaluationError::Unavailable(
                                VariableUnavailableReason::Other(
                                    DereferenceUnavailableReason::Null.to_string().into(),
                                ),
                            ));
                        }
                        LocatedStorage::Memory(pointer)
                    }
                }
                PathStep::Member(step) => {
                    let PlannedMemberStep {
                        aggregate,
                        child,
                        member,
                        required_variant,
                    } = *step;
                    if let Some((required, discriminant, variants)) = required_variant {
                        let active = match self.active_variant_from_storage(
                            &storage,
                            aggregate,
                            &discriminant,
                            &variants,
                            runtime,
                            &mut budget,
                        ) {
                            Ok(active) => active,
                            Err(error) => return failure(error),
                        };
                        if active != Some(required) {
                            let name = variants
                                .get(required)
                                .and_then(|variant| variant.name.as_deref())
                                .unwrap_or("<anonymous>");
                            return failure(PathEvaluationError::Unavailable(
                                VariableUnavailableReason::Other(
                                    format!("variant member belongs to inactive arm '{name}'")
                                        .into(),
                                ),
                            ));
                        }
                    }
                    match member.layout {
                        RecordMemberLayout::ByteOffset(offset) => {
                            match i64::try_from(offset)
                                .map_err(|_| VariableUnavailableReason::EvaluationLimit.into())
                                .and_then(|offset| Self::storage_with_offset(storage, offset))
                            {
                                Ok(storage) => storage,
                                Err(error) => return failure(error),
                            }
                        }
                        RecordMemberLayout::BitRange {
                            bit_offset,
                            bit_size,
                        } => match self.bit_field_storage(
                            storage,
                            member.type_ref.id,
                            bit_offset,
                            bit_size,
                            runtime,
                            &mut budget,
                        ) {
                            Ok(storage) => storage,
                            Err(error) => return failure(error),
                        },
                        RecordMemberLayout::Runtime => match self.runtime_member_storage(
                            &storage,
                            aggregate,
                            child,
                            runtime,
                            &mut budget,
                        ) {
                            Ok(storage) => storage,
                            Err(error) => return failure(error),
                        },
                    }
                }
                PathStep::Unavailable(reason) => {
                    return failure(PathEvaluationError::Unavailable(reason));
                }
            };
        }
        let Some(terminal) = plan.terminal else {
            return failure(PathEvaluationError::Malformed(
                "an untyped expression unexpectedly reached materialization".into(),
            ));
        };
        let Some(terminal_type) = terminal_type else {
            return failure(PathEvaluationError::Malformed(
                "a typed expression unexpectedly lost its terminal type".into(),
            ));
        };
        self.materialize_inspected_value(
            terminal,
            terminal_type,
            &storage,
            context,
            runtime,
            budget,
        )
    }

    fn materialize_inspected_value(
        &self,
        type_id: TypeId,
        type_info: TypeInfo,
        storage: &LocatedStorage,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        mut budget: EvaluationBudget,
    ) -> InspectedValue {
        let shape = match self.value_shape(type_id) {
            Ok(shape) => shape,
            Err(ValueShapeError::Malformed(description)) => {
                return InspectedValue {
                    type_info: Some(type_info),
                    state: VariableState::Malformed(VariableMalformedReason { description }),
                };
            }
            Err(ValueShapeError::Unsupported(description)) => {
                return InspectedValue {
                    type_info: Some(type_info),
                    state: VariableState::Unavailable(description.into()),
                };
            }
        };
        let Ok(size) = usize::try_from(shape.byte_size()) else {
            return InspectedValue {
                type_info: Some(type_info),
                state: VariableState::Unavailable(VariableUnavailableReason::EvaluationLimit),
            };
        };
        let (source, raw) = match Self::read_storage(storage, size, runtime, &mut budget) {
            Ok(value) => value,
            Err(PathEvaluationError::Unavailable(reason)) => {
                return InspectedValue {
                    type_info: Some(type_info),
                    state: VariableState::Unavailable(reason),
                };
            }
            Err(PathEvaluationError::Malformed(description)) => {
                return InspectedValue {
                    type_info: Some(type_info),
                    state: VariableState::Malformed(VariableMalformedReason { description }),
                };
            }
        };
        let mut state = if matches!(shape.kind, ValueShapeKind::Slice { .. }) {
            decode_slice_state(
                &self.types,
                &self.dynamic_record_layouts,
                &self.evaluation_units,
                self.endian,
                &shape,
                source,
                raw,
                self.target,
                runtime,
                budget,
            )
        } else {
            decode_value_state(
                &self.types,
                &self.dynamic_record_layouts,
                &self.evaluation_units,
                self.endian,
                &shape,
                context,
                source,
                raw,
                self.target,
                runtime,
                budget,
            )
        };
        self.constrain_dereference(&mut state, &shape);
        InspectedValue {
            type_info: Some(type_info),
            state,
        }
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
            Err(ValueShapeError::Malformed(description)) => {
                return malformed(variable, Some(type_info), description);
            }
            Err(ValueShapeError::Unsupported(description)) => {
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
                runtime,
                EvaluationBudget::default(),
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
        self.available_variable(
            variable, type_info, &shape, context, source, raw, runtime, budget,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "value materialization keeps source, context, and runtime explicit"
    )]
    fn available_variable(
        &self,
        variable: &CatalogDataObject,
        type_info: TypeInfo,
        shape: &ValueShape,
        context: VariableContext,
        source: VariableValueSource,
        raw: Arc<[u8]>,
        runtime: &mut dyn VariableRuntime,
        budget: EvaluationBudget,
    ) -> Variable {
        let mut value = if matches!(&shape.kind, ValueShapeKind::Slice { .. }) {
            Variable {
                kind: variable.kind,
                global: None,
                name: Arc::clone(&variable.name),
                declaration: variable.declaration.clone(),
                type_info: Some(type_info),
                state: decode_slice_state(
                    &self.types,
                    &self.dynamic_record_layouts,
                    &self.evaluation_units,
                    self.endian,
                    shape,
                    source,
                    raw,
                    self.target,
                    runtime,
                    budget,
                ),
            }
        } else {
            available(
                &self.types,
                &self.dynamic_record_layouts,
                &self.evaluation_units,
                self.endian,
                variable,
                type_info,
                shape,
                context,
                source,
                raw,
                self.target,
                runtime,
                budget,
            )
        };
        self.constrain_dereference(&mut value.state, shape);
        value
    }

    fn constrain_dereference(&self, state: &mut VariableState, shape: &ValueShape) {
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
        let pointee = self.type_info(*target).ok().cloned();
        match dereference {
            DereferenceState::Available(_) => {
                let reason = match self.type_info(*target) {
                    Err(description) => Some(DereferenceUnavailableReason::Malformed(
                        VariableMalformedReason { description },
                    )),
                    Ok(TypeInfo {
                        kind: TypeKind::Unspecified,
                        ..
                    }) => Some(DereferenceUnavailableReason::UnspecifiedPointee),
                    Ok(_) => match self.value_shape(*target) {
                        Ok(_) => None,
                        Err(ValueShapeError::Malformed(description)) => {
                            Some(DereferenceUnavailableReason::Malformed(
                                VariableMalformedReason { description },
                            ))
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
            Err(ValueShapeError::Malformed(description)) => {
                return Ok(DereferencedValue {
                    type_info,
                    state: VariableState::Malformed(VariableMalformedReason { description }),
                });
            }
            Err(ValueShapeError::Unsupported(description)) => {
                return Ok(DereferencedValue {
                    type_info,
                    state: VariableState::Unavailable(description.into()),
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
        let mut budget = EvaluationBudget::default();
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
                if budget.consume_memory(size).is_err() {
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
        let mut state = decode_value_state(
            &self.types,
            &self.dynamic_record_layouts,
            &self.evaluation_units,
            self.endian,
            &shape,
            context,
            raw.0,
            raw.1,
            self.target,
            runtime,
            budget,
        );
        self.constrain_dereference(&mut state, &shape);
        Ok(DereferencedValue { type_info, state })
    }
}

fn implicit_pointer_bytes(
    raw: &[u8],
    byte_offset: i64,
    size: usize,
) -> std::result::Result<Arc<[u8]>, VariableUnavailableReason> {
    let containing_size =
        u64::try_from(raw.len()).map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
    let size = u64::try_from(size).map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
    let (start, end) = implicit_pointer_range(byte_offset, size, containing_size)?;
    let start = usize::try_from(start).map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
    let end = usize::try_from(end).map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
    Ok(Arc::from(&raw[start..end]))
}

fn implicit_pointer_range(
    byte_offset: i64,
    size: u64,
    containing_size: u64,
) -> std::result::Result<(u64, u64), VariableUnavailableReason> {
    let start = u64::try_from(byte_offset).map_err(|_| {
        VariableUnavailableReason::Other(
            "negative implicit-pointer offsets outside the referenced object are unsupported"
                .into(),
        )
    })?;
    let end = start
        .checked_add(size)
        .ok_or(VariableUnavailableReason::EvaluationLimit)?;
    if end > containing_size {
        return Err(VariableUnavailableReason::Other(
            "implicit-pointer offset is outside the referenced value".into(),
        ));
    }
    Ok((start, end))
}

fn available_implicit_pointer(
    variable: &CatalogDataObject,
    type_info: TypeInfo,
    shape: &ValueShape,
    context: VariableContext,
    location: ImplicitPointerLocation,
) -> Variable {
    let state = match &shape.kind {
        ValueShapeKind::Indirection {
            target,
            byte_size,
            address_class,
        } if location
            .size_in_bits
            .is_none_or(|bits| bits == byte_size.saturating_mul(8))
            && location.bit_offset.is_none() =>
        {
            let dereference = if *address_class != 0 {
                DereferenceState::Unavailable {
                    pointee: None,
                    reason: DereferenceUnavailableReason::AddressClass(*address_class),
                }
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
                DereferenceState::Unavailable {
                    pointee: None,
                    reason: DereferenceUnavailableReason::UnspecifiedPointee,
                }
            };
            VariableState::Available {
                source: VariableValueSource::ImplicitPointer,
                raw: None,
                value: singleton_value_graph(
                    shape.type_info.clone(),
                    VariableValue::ImplicitPointer,
                ),
                dereference,
            }
        }
        ValueShapeKind::Array { .. }
        | ValueShapeKind::Slice { .. }
        | ValueShapeKind::Record { .. }
        | ValueShapeKind::Union { .. }
        | ValueShapeKind::Variant { .. }
        | ValueShapeKind::Indirection { .. } => {
            VariableState::Unavailable(crate::UnsupportedVariableFeature::CompositeLocation.into())
        }
        ValueShapeKind::Scalar(_) | ValueShapeKind::Enumeration { .. } => {
            VariableState::Malformed(VariableMalformedReason {
                description: "DW_OP_implicit_pointer described a non-pointer value".into(),
            })
        }
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

    const fn scalar(&self) -> Option<&BaseType> {
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

    const fn record_id(&self) -> Option<TypeId> {
        match self.kind {
            ValueShapeKind::Record { record, .. } => Some(record),
            ValueShapeKind::Union { union, .. } => Some(union),
            ValueShapeKind::Variant { aggregate, .. } => Some(aggregate),
            ValueShapeKind::Scalar(_)
            | ValueShapeKind::Enumeration { .. }
            | ValueShapeKind::Indirection { .. }
            | ValueShapeKind::Array { .. }
            | ValueShapeKind::Slice { .. } => None,
        }
    }

    const fn storage_type_id(&self) -> TypeId {
        match self.record_id() {
            Some(record) => record,
            None => self.type_info.reference.id,
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "materialization keeps provider state, stop context, source, target, and runtime explicit"
)]
fn available(
    types: &[TypeNode],
    dynamic_record_layouts: &HashMap<DynamicAggregateLayoutKey, Expression>,
    evaluation_units: &[EvaluationUnit],
    endian: RunTimeEndian,
    variable: &CatalogDataObject,
    type_info: TypeInfo,
    shape: &ValueShape,
    context: VariableContext,
    source: VariableValueSource,
    raw: Arc<[u8]>,
    target: TargetDescription,
    runtime: &mut dyn VariableRuntime,
    budget: EvaluationBudget,
) -> Variable {
    let state = decode_value_state(
        types,
        dynamic_record_layouts,
        evaluation_units,
        endian,
        shape,
        context,
        source,
        raw,
        target,
        runtime,
        budget,
    );
    Variable {
        kind: variable.kind,
        global: None,
        name: Arc::clone(&variable.name),
        declaration: variable.declaration.clone(),
        type_info: Some(type_info),
        state,
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "decoding keeps immutable provider inputs and the live runtime explicit"
)]
fn decode_value_state(
    types: &[TypeNode],
    dynamic_record_layouts: &HashMap<DynamicAggregateLayoutKey, Expression>,
    evaluation_units: &[EvaluationUnit],
    endian: RunTimeEndian,
    shape: &ValueShape,
    context: VariableContext,
    source: VariableValueSource,
    raw: Arc<[u8]>,
    target: TargetDescription,
    runtime: &mut dyn VariableRuntime,
    budget: EvaluationBudget,
) -> VariableState {
    match &shape.kind {
        ValueShapeKind::Scalar(_)
        | ValueShapeKind::Enumeration { .. }
        | ValueShapeKind::Array { .. }
        | ValueShapeKind::Record { .. }
        | ValueShapeKind::Union { .. }
        | ValueShapeKind::Variant { .. } => {
            match decode_value_graph_live(
                types,
                dynamic_record_layouts,
                evaluation_units,
                endian,
                shape,
                Arc::clone(&raw),
                &source,
                target,
                runtime,
                budget,
            ) {
                Ok(value) => VariableState::Available {
                    source,
                    raw: Some(raw),
                    value,
                    dereference: DereferenceState::NotApplicable,
                },
                Err(reason) => VariableState::Unavailable(reason),
            }
        }
        ValueShapeKind::Slice { .. } => VariableState::Malformed(VariableMalformedReason {
            description: "slice decoding requires a runtime read context".into(),
        }),
        ValueShapeKind::Indirection {
            target: target_type,
            byte_size,
            address_class,
        } => match decode_address(&raw, *byte_size, target) {
            Ok(address) => {
                let dereference = if *address_class != 0 {
                    DereferenceState::Unavailable {
                        pointee: None,
                        reason: DereferenceUnavailableReason::AddressClass(*address_class),
                    }
                } else if address.get() == 0 {
                    DereferenceState::Unavailable {
                        pointee: None,
                        reason: DereferenceUnavailableReason::Null,
                    }
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
                    DereferenceState::Unavailable {
                        pointee: None,
                        reason: DereferenceUnavailableReason::UnspecifiedPointee,
                    }
                };
                let value = match decode_value_graph(types, shape, Arc::clone(&raw), target) {
                    Ok(value) => value,
                    Err(reason) => return VariableState::Unavailable(reason),
                };
                VariableState::Available {
                    source,
                    raw: Some(raw),
                    value,
                    dereference,
                }
            }
            Err(reason) => VariableState::Unavailable(reason),
        },
    }
}

#[expect(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "slice decoding keeps provider state, budget, descriptor, backing read, graph, and provenance explicit"
)]
fn decode_slice_state<T: TypeMetadataEntry>(
    types: &[T],
    dynamic_record_layouts: &HashMap<DynamicAggregateLayoutKey, Expression>,
    evaluation_units: &[EvaluationUnit],
    endian: RunTimeEndian,
    shape: &ValueShape,
    source: VariableValueSource,
    raw: Arc<[u8]>,
    target: TargetDescription,
    runtime: &mut dyn VariableRuntime,
    mut budget: EvaluationBudget,
) -> VariableState {
    let ValueShapeKind::Slice {
        element,
        has_capacity,
        ..
    } = &shape.kind
    else {
        return VariableState::Malformed(VariableMalformedReason {
            description: "slice decoder received a non-slice type".into(),
        });
    };
    let pointer_bytes = match target.pointer_width {
        crate::PointerWidth::Bits32 => 4,
        crate::PointerWidth::Bits64 => 8,
    };
    let words = if *has_capacity { 3 } else { 2 };
    if raw.len() != pointer_bytes * words {
        return VariableState::Malformed(VariableMalformedReason {
            description: "slice descriptor size does not match its target layout".into(),
        });
    }
    let word = |index: usize| {
        unsigned_value(
            &raw[index * pointer_bytes..(index + 1) * pointer_bytes],
            target.byte_order,
        )
        .and_then(|value| {
            u64::try_from(value).map_err(|_| VariableUnavailableReason::EvaluationLimit)
        })
    };
    let address = match word(0) {
        Ok(value) => VirtualAddress::new(value),
        Err(reason) => return VariableState::Unavailable(reason),
    };
    let length = match word(1) {
        Ok(value) => value,
        Err(reason) => return VariableState::Unavailable(reason),
    };
    let capacity = if *has_capacity {
        match word(2) {
            Ok(value) if value >= length => Some(value),
            Ok(_) => {
                return VariableState::Malformed(VariableMalformedReason {
                    description: "slice length exceeds its capacity".into(),
                });
            }
            Err(reason) => return VariableState::Unavailable(reason),
        }
    } else {
        None
    };
    let element = match value_shape_from(types, *element) {
        Ok(element) => element,
        Err(ValueShapeError::Malformed(reason) | ValueShapeError::Unsupported(reason)) => {
            return VariableState::Unavailable(VariableUnavailableReason::Other(reason));
        }
    };
    let Some(byte_size) = length.checked_mul(element.byte_size()) else {
        return VariableState::Unavailable(VariableUnavailableReason::EvaluationLimit);
    };
    let size = match usize::try_from(byte_size) {
        Ok(size) if size <= MAX_EVALUATION_MEMORY_BYTES => size,
        _ => return VariableState::Unavailable(VariableUnavailableReason::EvaluationLimit),
    };
    if address.get() == 0 && size != 0 {
        return VariableState::Malformed(VariableMalformedReason {
            description: "non-empty slice has a null data pointer".into(),
        });
    }
    let backing = if size == 0 {
        Arc::from([])
    } else {
        if let Err(reason) = budget.consume_memory(size) {
            return VariableState::Unavailable(reason);
        }
        match runtime.read_memory(address, size) {
            Ok(bytes) => bytes,
            Err(reason) => return VariableState::Unavailable(reason.into()),
        }
    };
    let stride = match usize::try_from(element.byte_size()) {
        Ok(stride) if stride != 0 => stride,
        _ => return VariableState::Unavailable(VariableUnavailableReason::EvaluationLimit),
    };
    let mut builder = ValueGraphBuilder::new(types, shape.type_info.clone());
    let available_nodes = MAX_VALUE_NODES.saturating_sub(builder.nodes.len());
    let materialized = backing.chunks_exact(stride).len().min(available_nodes);
    let mut elements = Vec::with_capacity(materialized);
    for (index, _) in backing.chunks_exact(stride).take(materialized).enumerate() {
        let Some(start) = index.checked_mul(stride) else {
            return VariableState::Unavailable(VariableUnavailableReason::EvaluationLimit);
        };
        let Some(end) = start.checked_add(stride) else {
            return VariableState::Unavailable(VariableUnavailableReason::EvaluationLimit);
        };
        let id = match builder.allocate(element.type_info.clone()) {
            Ok(id) => id,
            Err(reason) => return VariableState::Unavailable(reason),
        };
        elements.push(id);
        builder.schedule(
            id,
            element.clone(),
            Arc::clone(&backing),
            start,
            end,
            1,
            address
                .get()
                .checked_add(u64::try_from(start).unwrap_or(u64::MAX))
                .map(VirtualAddress::new),
            Arc::from([]),
        );
    }
    builder.set(
        ValueNodeId::new(0),
        ValueNodeState::Available(VariableValue::Slice {
            length,
            capacity,
            elements: elements.into(),
            omitted: length.saturating_sub(u64::try_from(materialized).unwrap_or(u64::MAX)),
        }),
    );
    let mut dynamic = DynamicDecodeContext {
        layouts: dynamic_record_layouts,
        units: evaluation_units,
        endian,
        runtime,
        budget,
    };
    if let Err(reason) = builder.run(target, Some(&mut dynamic)) {
        return VariableState::Unavailable(reason);
    }
    let value = match builder.finish() {
        Ok(value) => value,
        Err(reason) => return VariableState::Unavailable(reason),
    };
    VariableState::Available {
        source,
        raw: Some(raw),
        value,
        dereference: DereferenceState::NotApplicable,
    }
}

struct PendingValueNode {
    id: ValueNodeId,
    shape: ValueShape,
    raw: Arc<[u8]>,
    start: usize,
    end: usize,
    depth: usize,
    address: Option<VirtualAddress>,
    record_ancestors: Arc<[TypeId]>,
}

struct DynamicDecodeContext<'a> {
    layouts: &'a HashMap<DynamicAggregateLayoutKey, Expression>,
    units: &'a [EvaluationUnit],
    endian: RunTimeEndian,
    runtime: &'a mut dyn VariableRuntime,
    budget: EvaluationBudget,
}

struct ValueGraphBuilder<'a, T> {
    types: &'a [T],
    nodes: Vec<ValueNode>,
    pending: Vec<PendingValueNode>,
    expanded_storage: HashMap<(TypeId, u64, u64), ValueNodeId>,
}

impl<'a, T: TypeMetadataEntry> ValueGraphBuilder<'a, T> {
    fn new(types: &'a [T], root_type: TypeInfo) -> Self {
        Self {
            types,
            nodes: vec![ValueNode {
                type_info: root_type,
                state: ValueNodeState::Truncated(InspectionLimit::ValueNodes),
            }],
            pending: Vec::new(),
            expanded_storage: HashMap::new(),
        }
    }

    fn allocate(
        &mut self,
        type_info: TypeInfo,
    ) -> std::result::Result<ValueNodeId, VariableUnavailableReason> {
        if self.nodes.len() >= MAX_VALUE_NODES {
            return Err(VariableUnavailableReason::EvaluationLimit);
        }
        let id = ValueNodeId::new(
            u32::try_from(self.nodes.len())
                .map_err(|_| VariableUnavailableReason::EvaluationLimit)?,
        );
        self.nodes.push(ValueNode {
            type_info,
            state: ValueNodeState::Truncated(InspectionLimit::ValueNodes),
        });
        Ok(id)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "pending nodes keep typed storage bounds, depth, and address explicit"
    )]
    fn schedule(
        &mut self,
        id: ValueNodeId,
        shape: ValueShape,
        raw: Arc<[u8]>,
        start: usize,
        end: usize,
        depth: usize,
        address: Option<VirtualAddress>,
        record_ancestors: Arc<[TypeId]>,
    ) {
        let record_ancestors = if let Some(record) = shape.record_id() {
            if record_ancestors.contains(&record) {
                self.set(
                    id,
                    ValueNodeState::Malformed(VariableMalformedReason {
                        description: "record type graph contains a positive by-value cycle".into(),
                    }),
                );
                return;
            }
            let mut ancestors = record_ancestors.to_vec();
            ancestors.push(record);
            ancestors.into()
        } else {
            record_ancestors
        };
        self.pending.push(PendingValueNode {
            id,
            shape,
            raw,
            start,
            end,
            depth,
            address,
            record_ancestors,
        });
    }

    fn set(&mut self, id: ValueNodeId, state: ValueNodeState) {
        self.nodes[usize::try_from(id.get()).expect("value node ID fits usize")].state = state;
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the iterative graph reducer handles each normalized value shape in one work loop"
    )]
    fn run(
        &mut self,
        target: TargetDescription,
        mut dynamic: Option<&mut DynamicDecodeContext<'_>>,
    ) -> std::result::Result<(), VariableUnavailableReason> {
        while let Some(pending) = self.pending.pop() {
            let Some(raw) = pending.raw.get(pending.start..pending.end) else {
                self.set(
                    pending.id,
                    ValueNodeState::Unavailable(VariableUnavailableReason::Other(
                        "aggregate storage is shorter than its type".into(),
                    )),
                );
                continue;
            };
            match &pending.shape.kind {
                ValueShapeKind::Scalar(base) => {
                    self.set(
                        pending.id,
                        match decode_scalar(base, raw, target) {
                            Ok(value) => ValueNodeState::Available(VariableValue::Scalar(value)),
                            Err(reason) => ValueNodeState::Unavailable(reason),
                        },
                    );
                }
                ValueShapeKind::Enumeration {
                    representation,
                    enumerators,
                    ..
                } => {
                    self.set(
                        pending.id,
                        match decode_integer_value(representation, raw, target.byte_order) {
                            Ok(value) => {
                                let matches = enumerators
                                    .iter()
                                    .filter(|enumerator| enumerator.value == value)
                                    .cloned()
                                    .collect::<Vec<_>>()
                                    .into();
                                ValueNodeState::Available(VariableValue::Enumeration {
                                    value,
                                    matches,
                                })
                            }
                            Err(reason) => ValueNodeState::Unavailable(reason.into()),
                        },
                    );
                }
                ValueShapeKind::Indirection { byte_size, .. } => {
                    self.set(
                        pending.id,
                        match decode_address(raw, *byte_size, target) {
                            Ok(address) => {
                                ValueNodeState::Available(VariableValue::Address(AddressValue {
                                    address,
                                }))
                            }
                            Err(reason) => ValueNodeState::Unavailable(reason),
                        },
                    );
                }
                ValueShapeKind::Array {
                    element,
                    dimensions,
                    ..
                } => {
                    let element =
                        value_shape_from(self.types, *element).map_err(|error| match error {
                            ValueShapeError::Malformed(reason)
                            | ValueShapeError::Unsupported(reason) => {
                                VariableUnavailableReason::Other(reason)
                            }
                        })?;
                    let count = dimensions
                        .iter()
                        .try_fold(1_u64, |count, dimension| count.checked_mul(dimension.count))
                        .ok_or(VariableUnavailableReason::EvaluationLimit)?;
                    let stride = usize::try_from(element.byte_size())
                        .map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
                    let count = usize::try_from(count)
                        .map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
                    let materialized = count.min(MAX_VALUE_NODES.saturating_sub(self.nodes.len()));
                    let mut elements = Vec::with_capacity(materialized);
                    for index in 0..materialized {
                        let start = pending
                            .start
                            .checked_add(
                                index
                                    .checked_mul(stride)
                                    .ok_or(VariableUnavailableReason::EvaluationLimit)?,
                            )
                            .ok_or(VariableUnavailableReason::EvaluationLimit)?;
                        let end = start
                            .checked_add(stride)
                            .ok_or(VariableUnavailableReason::EvaluationLimit)?;
                        let id = self.allocate(element.type_info.clone())?;
                        elements.push(id);
                        self.schedule(
                            id,
                            element.clone(),
                            Arc::clone(&pending.raw),
                            start,
                            end,
                            pending.depth.saturating_add(1),
                            pending.address.and_then(|address| {
                                u64::try_from(index.checked_mul(stride)?)
                                    .ok()
                                    .and_then(|offset| address.get().checked_add(offset))
                                    .map(VirtualAddress::new)
                            }),
                            Arc::clone(&pending.record_ancestors),
                        );
                    }
                    self.set(
                        pending.id,
                        ValueNodeState::Available(VariableValue::Array {
                            dimensions: Arc::clone(dimensions),
                            elements: elements.into(),
                            omitted: u64::try_from(count.saturating_sub(materialized))
                                .unwrap_or(u64::MAX),
                        }),
                    );
                }
                ValueShapeKind::Slice { .. } => self.set(
                    pending.id,
                    ValueNodeState::Unavailable(
                        crate::UnsupportedVariableFeature::CompositeLocation.into(),
                    ),
                ),
                ValueShapeKind::Record {
                    record,
                    members,
                    bases,
                    ..
                } => {
                    let mut member_values = Vec::with_capacity(members.len());
                    for (index, member) in members.iter().enumerate() {
                        if self.nodes.len() >= MAX_VALUE_NODES {
                            continue;
                        }
                        let child = self.record_child(
                            &pending,
                            member.type_ref.id,
                            member.layout,
                            target,
                            DynamicAggregateLayoutKey {
                                aggregate: *record,
                                child: DynamicAggregateChild::Member(index),
                            },
                            dynamic.as_deref_mut(),
                        )?;
                        member_values.push(RecordMemberValue {
                            member: member.clone(),
                            value: child,
                        });
                    }
                    let mut base_values = Vec::with_capacity(bases.len());
                    for (index, base) in bases.iter().enumerate() {
                        if self.nodes.len() >= MAX_VALUE_NODES {
                            continue;
                        }
                        let child = self.record_child(
                            &pending,
                            base.type_ref.id,
                            base.layout,
                            target,
                            DynamicAggregateLayoutKey {
                                aggregate: *record,
                                child: DynamicAggregateChild::Base(index),
                            },
                            dynamic.as_deref_mut(),
                        )?;
                        base_values.push(BaseClassValue {
                            base: base.clone(),
                            value: child,
                        });
                    }
                    let omitted = u64::try_from(
                        members
                            .len()
                            .saturating_add(bases.len())
                            .saturating_sub(member_values.len().saturating_add(base_values.len())),
                    )
                    .unwrap_or(u64::MAX);
                    self.set(
                        pending.id,
                        ValueNodeState::Available(VariableValue::Record {
                            members: member_values.into(),
                            bases: base_values.into(),
                            omitted,
                        }),
                    );
                }
                ValueShapeKind::Union { union, members, .. } => {
                    let mut member_values = Vec::with_capacity(members.len());
                    for (index, member) in members.iter().enumerate() {
                        if self.nodes.len() >= MAX_VALUE_NODES {
                            continue;
                        }
                        let child = self.record_child(
                            &pending,
                            member.type_ref.id,
                            member.layout,
                            target,
                            DynamicAggregateLayoutKey {
                                aggregate: *union,
                                child: DynamicAggregateChild::Member(index),
                            },
                            dynamic.as_deref_mut(),
                        )?;
                        member_values.push(RecordMemberValue {
                            member: member.clone(),
                            value: child,
                        });
                    }
                    let omitted = u64::try_from(members.len().saturating_sub(member_values.len()))
                        .unwrap_or(u64::MAX);
                    self.set(
                        pending.id,
                        ValueNodeState::Available(VariableValue::Union {
                            members: member_values.into(),
                            omitted,
                        }),
                    );
                }
                ValueShapeKind::Variant {
                    aggregate,
                    common_members,
                    bases,
                    discriminant,
                    variants,
                    ..
                } => {
                    let (discriminant_value, active_index) = match discriminant {
                        VariantDiscriminant::Stored(member) => {
                            let value = match self.decode_variant_discriminant(
                                &pending,
                                *aggregate,
                                member,
                                target,
                                dynamic.as_deref_mut(),
                            ) {
                                Ok(value) => value,
                                Err(PathEvaluationError::Unavailable(reason)) => {
                                    self.set(pending.id, ValueNodeState::Unavailable(reason));
                                    continue;
                                }
                                Err(PathEvaluationError::Malformed(description)) => {
                                    self.set(
                                        pending.id,
                                        ValueNodeState::Malformed(VariableMalformedReason {
                                            description,
                                        }),
                                    );
                                    continue;
                                }
                            };
                            let active = match selected_variant_index(variants, value) {
                                Ok(active) => active,
                                Err(description) => {
                                    self.set(
                                        pending.id,
                                        ValueNodeState::Malformed(VariableMalformedReason {
                                            description,
                                        }),
                                    );
                                    continue;
                                }
                            };
                            (Some(value), active)
                        }
                        VariantDiscriminant::TagType(_) => (None, Some(0)),
                    };

                    let mut common_values = Vec::with_capacity(common_members.len());
                    for (index, member) in common_members.iter().enumerate() {
                        if self.nodes.len() >= MAX_VALUE_NODES {
                            continue;
                        }
                        let child = self.record_child(
                            &pending,
                            member.type_ref.id,
                            member.layout,
                            target,
                            DynamicAggregateLayoutKey {
                                aggregate: *aggregate,
                                child: DynamicAggregateChild::Member(index),
                            },
                            dynamic.as_deref_mut(),
                        )?;
                        common_values.push(RecordMemberValue {
                            member: member.clone(),
                            value: child,
                        });
                    }
                    let mut base_values = Vec::with_capacity(bases.len());
                    for (index, base) in bases.iter().enumerate() {
                        if self.nodes.len() >= MAX_VALUE_NODES {
                            continue;
                        }
                        let child = self.record_child(
                            &pending,
                            base.type_ref.id,
                            base.layout,
                            target,
                            DynamicAggregateLayoutKey {
                                aggregate: *aggregate,
                                child: DynamicAggregateChild::Base(index),
                            },
                            dynamic.as_deref_mut(),
                        )?;
                        base_values.push(BaseClassValue {
                            base: base.clone(),
                            value: child,
                        });
                    }
                    let active = if let Some(active_index) = active_index {
                        let variant = variants
                            .get(active_index)
                            .expect("validated variant index")
                            .clone();
                        let mut member_values = Vec::with_capacity(variant.members.len());
                        for (member_index, member) in variant.members.iter().enumerate() {
                            if self.nodes.len() >= MAX_VALUE_NODES {
                                continue;
                            }
                            let child = self.record_child(
                                &pending,
                                member.type_ref.id,
                                member.layout,
                                target,
                                DynamicAggregateLayoutKey {
                                    aggregate: *aggregate,
                                    child: DynamicAggregateChild::VariantMember {
                                        variant: active_index,
                                        member: member_index,
                                    },
                                },
                                dynamic.as_deref_mut(),
                            )?;
                            member_values.push(RecordMemberValue {
                                member: member.clone(),
                                value: child,
                            });
                        }
                        let omitted = u64::try_from(
                            variant.members.len().saturating_sub(member_values.len()),
                        )
                        .unwrap_or(u64::MAX);
                        Some(ActiveVariantValue {
                            variant,
                            members: member_values.into(),
                            omitted,
                        })
                    } else {
                        None
                    };
                    let omitted = u64::try_from(
                        common_members
                            .len()
                            .saturating_add(bases.len())
                            .saturating_sub(common_values.len().saturating_add(base_values.len())),
                    )
                    .unwrap_or(u64::MAX);
                    self.set(
                        pending.id,
                        ValueNodeState::Available(VariableValue::Variant {
                            discriminant: discriminant_value,
                            common_members: common_values.into(),
                            bases: base_values.into(),
                            active,
                            omitted,
                        }),
                    );
                }
            }
        }
        Ok(())
    }

    fn record_child(
        &mut self,
        parent: &PendingValueNode,
        type_id: TypeId,
        layout: RecordMemberLayout,
        target: TargetDescription,
        dynamic_key: DynamicAggregateLayoutKey,
        dynamic: Option<&mut DynamicDecodeContext<'_>>,
    ) -> std::result::Result<ValueNodeId, VariableUnavailableReason> {
        let type_info = type_info_from(self.types, type_id)
            .map_err(VariableUnavailableReason::Other)?
            .clone();
        let id = self.allocate(type_info)?;
        if parent.depth >= MAX_AGGREGATE_DEPTH {
            self.set(
                id,
                ValueNodeState::Truncated(InspectionLimit::AggregateDepth),
            );
            return Ok(id);
        }
        if let RecordMemberLayout::BitRange {
            bit_offset,
            bit_size,
        } = layout
        {
            self.decode_bit_field(id, parent, type_id, bit_offset, bit_size, target)?;
            return Ok(id);
        }
        let RecordMemberLayout::ByteOffset(offset) = layout else {
            self.decode_dynamic_record_child(id, parent, type_id, dynamic_key, dynamic, target)?;
            return Ok(id);
        };
        let shape = match value_shape_from(self.types, type_id) {
            Ok(shape) => shape,
            Err(ValueShapeError::Unsupported(reason)) => {
                self.set(
                    id,
                    ValueNodeState::Unavailable(VariableUnavailableReason::Other(reason)),
                );
                return Ok(id);
            }
            Err(ValueShapeError::Malformed(description)) => {
                self.set(
                    id,
                    ValueNodeState::Malformed(VariableMalformedReason { description }),
                );
                return Ok(id);
            }
        };
        let offset =
            usize::try_from(offset).map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
        let size = usize::try_from(shape.byte_size())
            .map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
        let start = parent
            .start
            .checked_add(offset)
            .ok_or(VariableUnavailableReason::EvaluationLimit)?;
        let end = start
            .checked_add(size)
            .ok_or(VariableUnavailableReason::EvaluationLimit)?;
        if end > parent.end {
            self.set(
                id,
                ValueNodeState::Malformed(VariableMalformedReason {
                    description: "record member extends beyond its containing object".into(),
                }),
            );
        } else {
            self.schedule(
                id,
                shape,
                Arc::clone(&parent.raw),
                start,
                end,
                parent.depth.saturating_add(1),
                parent.address.and_then(|address| {
                    address
                        .get()
                        .checked_add(u64::try_from(offset).ok()?)
                        .map(VirtualAddress::new)
                }),
                Arc::clone(&parent.record_ancestors),
            );
        }
        Ok(id)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "static, bit-field, and runtime discriminator reads preserve distinct typed failures"
    )]
    fn decode_variant_discriminant(
        &self,
        parent: &PendingValueNode,
        aggregate: TypeId,
        member: &RecordMember,
        target: TargetDescription,
        dynamic: Option<&mut DynamicDecodeContext<'_>>,
    ) -> std::result::Result<IntegerValue, PathEvaluationError> {
        let shape =
            value_shape_from(self.types, member.type_ref.id).map_err(|error| match error {
                ValueShapeError::Malformed(reason) => PathEvaluationError::Malformed(reason),
                ValueShapeError::Unsupported(reason) => {
                    PathEvaluationError::Unavailable(VariableUnavailableReason::Other(reason))
                }
            })?;
        let mut representation = match &shape.kind {
            ValueShapeKind::Scalar(base)
                if !matches!(base.encoding, BaseTypeEncoding::Floating) =>
            {
                base.clone()
            }
            ValueShapeKind::Enumeration { representation, .. } => representation.clone(),
            _ => {
                return Err(PathEvaluationError::Malformed(
                    "variant discriminator type is not integral".into(),
                ));
            }
        };
        match member.layout {
            RecordMemberLayout::ByteOffset(offset) => {
                let offset = usize::try_from(offset).map_err(|_| {
                    PathEvaluationError::Malformed(
                        "variant discriminator offset exceeds host usize".into(),
                    )
                })?;
                let size = usize::try_from(representation.byte_size).map_err(|_| {
                    PathEvaluationError::Malformed(
                        "variant discriminator size exceeds host usize".into(),
                    )
                })?;
                let start = parent.start.checked_add(offset).ok_or_else(|| {
                    PathEvaluationError::Malformed("variant discriminator offset overflows".into())
                })?;
                let end = start.checked_add(size).ok_or_else(|| {
                    PathEvaluationError::Malformed("variant discriminator range overflows".into())
                })?;
                if end > parent.end {
                    return Err(PathEvaluationError::Malformed(
                        "variant discriminator extends beyond its containing object".into(),
                    ));
                }
                decode_integer_value(&representation, &parent.raw[start..end], target.byte_order)
                    .map_err(PathEvaluationError::Malformed)
            }
            RecordMemberLayout::BitRange {
                bit_offset,
                bit_size,
            } => {
                let raw = extract_bit_field(
                    &parent.raw[parent.start..parent.end],
                    bit_offset,
                    bit_size,
                    target.byte_order,
                )
                .map_err(PathEvaluationError::Malformed)?;
                representation.bit_size = Some(bit_size);
                let size = usize::try_from(representation.byte_size).map_err(|_| {
                    PathEvaluationError::Malformed(
                        "variant discriminator byte size exceeds host usize".into(),
                    )
                })?;
                let full = raw.to_le_bytes();
                let mut bytes = full[..size].to_vec();
                if target.byte_order == ByteOrder::Big {
                    bytes.reverse();
                }
                decode_integer_value(&representation, &bytes, target.byte_order)
                    .map_err(PathEvaluationError::Malformed)
            }
            RecordMemberLayout::Runtime => {
                let Some(dynamic) = dynamic else {
                    return Err(PathEvaluationError::Unavailable(
                        VariableUnavailableReason::Other(
                            "runtime variant discriminator requires a live object address".into(),
                        ),
                    ));
                };
                let Some(object_address) = parent.address else {
                    return Err(PathEvaluationError::Unavailable(
                        VariableUnavailableReason::Other(
                            "runtime variant discriminator has no concrete containing-object address"
                                .into(),
                        ),
                    ));
                };
                let address = evaluate_dynamic_aggregate_address(
                    dynamic.layouts,
                    DynamicAggregateLayoutKey {
                        aggregate,
                        child: DynamicAggregateChild::Discriminant,
                    },
                    dynamic.endian,
                    dynamic.units,
                    dynamic.runtime,
                    &mut dynamic.budget,
                    object_address,
                )?;
                let size = usize::try_from(representation.byte_size)
                    .map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
                dynamic.budget.consume_memory(size)?;
                let raw = dynamic
                    .runtime
                    .read_memory(address, size)
                    .map_err(VariableUnavailableReason::Other)?;
                decode_integer_value(&representation, &raw, target.byte_order)
                    .map_err(PathEvaluationError::Malformed)
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "dynamic children preserve every evaluator, address, read, and child-state failure distinctly"
    )]
    fn decode_dynamic_record_child(
        &mut self,
        id: ValueNodeId,
        parent: &PendingValueNode,
        type_id: TypeId,
        key: DynamicAggregateLayoutKey,
        dynamic: Option<&mut DynamicDecodeContext<'_>>,
        target: TargetDescription,
    ) -> std::result::Result<(), VariableUnavailableReason> {
        let Some(dynamic) = dynamic else {
            self.set(
                id,
                ValueNodeState::Unavailable(VariableUnavailableReason::Other(
                    "runtime member location requires a live object address".into(),
                )),
            );
            return Ok(());
        };
        let Some(object_address) = parent.address else {
            self.set(
                id,
                ValueNodeState::Unavailable(VariableUnavailableReason::Other(
                    "runtime member location has no concrete containing-object address".into(),
                )),
            );
            return Ok(());
        };
        let Some(expression) = dynamic.layouts.get(&key) else {
            self.set(
                id,
                ValueNodeState::Unavailable(VariableUnavailableReason::Other(
                    "runtime member location form is unsupported".into(),
                )),
            );
            return Ok(());
        };
        let pieces = match evaluate_with_object(
            expression,
            dynamic.endian,
            &mut FrameBase::Unsupported,
            dynamic.units,
            dynamic.runtime,
            &mut dynamic.budget,
            Some(object_address),
        ) {
            Ok(pieces) => pieces,
            Err(EvaluateError::Unavailable(reason)) => {
                self.set(id, ValueNodeState::Unavailable(reason));
                return Ok(());
            }
            Err(EvaluateError::Malformed(description)) => {
                self.set(
                    id,
                    ValueNodeState::Malformed(VariableMalformedReason { description }),
                );
                return Ok(());
            }
        };
        let [piece] = pieces.as_slice() else {
            self.set(
                id,
                ValueNodeState::Malformed(VariableMalformedReason {
                    description: "runtime member location produced multiple pieces".into(),
                }),
            );
            return Ok(());
        };
        if piece.size_in_bits.is_some() || piece.bit_offset.is_some() {
            self.set(
                id,
                ValueNodeState::Unavailable(
                    crate::UnsupportedVariableFeature::CompositeLocation.into(),
                ),
            );
            return Ok(());
        }
        let Location::Address { address } = piece.location else {
            self.set(
                id,
                ValueNodeState::Malformed(VariableMalformedReason {
                    description: "runtime member location did not produce an address".into(),
                }),
            );
            return Ok(());
        };
        let shape = match value_shape_from(self.types, type_id) {
            Ok(shape) => shape,
            Err(ValueShapeError::Unsupported(reason)) => {
                self.set(id, ValueNodeState::Unavailable(reason.into()));
                return Ok(());
            }
            Err(ValueShapeError::Malformed(description)) => {
                self.set(
                    id,
                    ValueNodeState::Malformed(VariableMalformedReason { description }),
                );
                return Ok(());
            }
        };
        let size = usize::try_from(shape.byte_size())
            .map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
        let storage_key = (
            shape.storage_type_id(),
            address,
            u64::try_from(size).map_err(|_| VariableUnavailableReason::EvaluationLimit)?,
        );
        if let Some(original) = self.expanded_storage.get(&storage_key).copied() {
            self.set(id, ValueNodeState::Cycle { original });
            return Ok(());
        }
        self.expanded_storage.insert(storage_key, id);
        dynamic.budget.consume_memory(size)?;
        let address = VirtualAddress::new(address);
        let raw = dynamic
            .runtime
            .read_memory(address, size)
            .map_err(VariableUnavailableReason::Other)?;
        self.schedule(
            id,
            shape,
            raw,
            0,
            size,
            parent.depth.saturating_add(1),
            Some(address),
            Arc::clone(&parent.record_ancestors),
        );
        let _ = target;
        Ok(())
    }

    fn decode_bit_field(
        &mut self,
        id: ValueNodeId,
        parent: &PendingValueNode,
        type_id: TypeId,
        bit_offset: u64,
        bit_size: u64,
        target: TargetDescription,
    ) -> std::result::Result<(), VariableUnavailableReason> {
        let shape = match value_shape_from(self.types, type_id) {
            Ok(shape) => shape,
            Err(ValueShapeError::Unsupported(reason)) => {
                self.set(id, ValueNodeState::Unavailable(reason.into()));
                return Ok(());
            }
            Err(ValueShapeError::Malformed(description)) => {
                self.set(
                    id,
                    ValueNodeState::Malformed(VariableMalformedReason { description }),
                );
                return Ok(());
            }
        };
        let (base, enumerators) = match &shape.kind {
            ValueShapeKind::Scalar(base) => (base, None),
            ValueShapeKind::Enumeration {
                representation,
                enumerators,
                ..
            } => (representation, Some(enumerators)),
            _ => {
                self.set(
                    id,
                    ValueNodeState::Unavailable(VariableUnavailableReason::Other(
                        "non-integral bit-fields are unsupported".into(),
                    )),
                );
                return Ok(());
            }
        };
        let storage_bits = base
            .byte_size
            .checked_mul(8)
            .ok_or(VariableUnavailableReason::EvaluationLimit)?;
        if bit_size == 0 || bit_size > storage_bits || bit_size > 128 {
            self.set(
                id,
                ValueNodeState::Malformed(VariableMalformedReason {
                    description: "bit-field width exceeds its declared scalar storage".into(),
                }),
            );
            return Ok(());
        }
        let object = &parent.raw[parent.start..parent.end];
        let value = extract_bit_field(object, bit_offset, bit_size, target.byte_order)
            .map_err(VariableUnavailableReason::Other)?;
        let byte_size = usize::try_from(base.byte_size)
            .map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
        let full = value.to_le_bytes();
        let mut bytes = full[..byte_size].to_vec();
        if target.byte_order == ByteOrder::Big {
            bytes.reverse();
        }
        let mut narrowed = base.clone();
        narrowed.bit_size = Some(bit_size);
        let state = enumerators.map_or_else(
            || match decode_scalar(&narrowed, &bytes, target) {
                Ok(value) => ValueNodeState::Available(VariableValue::Scalar(value)),
                Err(reason) => ValueNodeState::Unavailable(reason),
            },
            |enumerators| match decode_integer_value(&narrowed, &bytes, target.byte_order) {
                Ok(value) => {
                    let matches = enumerators
                        .iter()
                        .filter(|enumerator| enumerator.value == value)
                        .cloned()
                        .collect::<Vec<_>>()
                        .into();
                    ValueNodeState::Available(VariableValue::Enumeration { value, matches })
                }
                Err(reason) => ValueNodeState::Unavailable(reason.into()),
            },
        );
        self.set(id, state);
        Ok(())
    }

    fn finish(self) -> std::result::Result<ValueGraph, VariableUnavailableReason> {
        ValueGraph::new(ValueNodeId::new(0), self.nodes.into())
            .ok_or_else(|| VariableUnavailableReason::Other("invalid value graph".into()))
    }
}

fn decode_value_graph<T: TypeMetadataEntry>(
    types: &[T],
    shape: &ValueShape,
    raw: Arc<[u8]>,
    target: TargetDescription,
) -> std::result::Result<ValueGraph, VariableUnavailableReason> {
    let mut builder = ValueGraphBuilder::new(types, shape.type_info.clone());
    let raw_len = raw.len();
    builder.schedule(
        ValueNodeId::new(0),
        shape.clone(),
        raw,
        0,
        raw_len,
        0,
        None,
        Arc::from([]),
    );
    builder.run(target, None)?;
    match &builder.nodes[0].state {
        ValueNodeState::Unavailable(reason) => return Err(reason.clone()),
        ValueNodeState::Malformed(reason) => {
            return Err(VariableUnavailableReason::Other(Arc::clone(
                &reason.description,
            )));
        }
        ValueNodeState::Available(_) | ValueNodeState::Truncated(_) => {}
        ValueNodeState::Cycle { .. } => {
            return Err(VariableUnavailableReason::Other(
                "value graph root cannot be a cycle".into(),
            ));
        }
    }
    builder.finish()
}

#[expect(
    clippy::too_many_arguments,
    reason = "record materialization keeps target, provider metadata, and live runtime explicit"
)]
fn decode_value_graph_live<T: TypeMetadataEntry>(
    types: &[T],
    dynamic_record_layouts: &HashMap<DynamicAggregateLayoutKey, Expression>,
    evaluation_units: &[EvaluationUnit],
    endian: RunTimeEndian,
    shape: &ValueShape,
    raw: Arc<[u8]>,
    source: &VariableValueSource,
    target: TargetDescription,
    runtime: &mut dyn VariableRuntime,
    budget: EvaluationBudget,
) -> std::result::Result<ValueGraph, VariableUnavailableReason> {
    let address = match source {
        VariableValueSource::Memory(address) => Some(*address),
        _ => None,
    };
    let mut builder = ValueGraphBuilder::new(types, shape.type_info.clone());
    if let Some(address) = address {
        builder.expanded_storage.insert(
            (shape.storage_type_id(), address.get(), shape.byte_size()),
            ValueNodeId::new(0),
        );
    }
    let raw_len = raw.len();
    builder.schedule(
        ValueNodeId::new(0),
        shape.clone(),
        raw,
        0,
        raw_len,
        0,
        address,
        Arc::from([]),
    );
    let mut dynamic = DynamicDecodeContext {
        layouts: dynamic_record_layouts,
        units: evaluation_units,
        endian,
        runtime,
        budget,
    };
    builder.run(target, Some(&mut dynamic))?;
    builder.finish()
}

fn extract_bit_field(
    bytes: &[u8],
    bit_offset: u64,
    bit_size: u64,
    byte_order: ByteOrder,
) -> std::result::Result<u128, Arc<str>> {
    let end = bit_offset
        .checked_add(bit_size)
        .ok_or_else(|| Arc::from("bit-field range overflows"))?;
    let available = u64::try_from(bytes.len())
        .ok()
        .and_then(|length| length.checked_mul(8))
        .ok_or_else(|| Arc::from("record storage size overflows"))?;
    if bit_size == 0 || bit_size > 128 || end > available {
        return Err("bit-field range is outside its containing object".into());
    }
    let mut value = 0_u128;
    for field_bit in 0..bit_size {
        let source = bit_offset + field_bit;
        let byte = bytes[usize::try_from(source / 8).expect("validated bit index fits usize")];
        let within = u32::try_from(source % 8).expect("bit index is below eight");
        let bit = match byte_order {
            ByteOrder::Little => (byte >> within) & 1,
            ByteOrder::Big => (byte >> (7 - within)) & 1,
        };
        match byte_order {
            ByteOrder::Little => value |= u128::from(bit) << field_bit,
            ByteOrder::Big => value = (value << 1) | u128::from(bit),
        }
    }
    Ok(value)
}

fn singleton_value_graph(type_info: TypeInfo, value: VariableValue) -> ValueGraph {
    ValueGraph::new(
        ValueNodeId::new(0),
        Arc::from([ValueNode {
            type_info,
            state: ValueNodeState::Available(value),
        }]),
    )
    .expect("a singleton value graph is valid")
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
    evaluate_with_object(expression, endian, frame_base, units, runtime, budget, None)
}

fn evaluate_dynamic_aggregate_address(
    layouts: &HashMap<DynamicAggregateLayoutKey, Expression>,
    key: DynamicAggregateLayoutKey,
    endian: RunTimeEndian,
    units: &[EvaluationUnit],
    runtime: &mut dyn VariableRuntime,
    budget: &mut EvaluationBudget,
    object_address: VirtualAddress,
) -> std::result::Result<VirtualAddress, PathEvaluationError> {
    let expression = layouts.get(&key).ok_or_else(|| {
        PathEvaluationError::Unavailable(VariableUnavailableReason::Other(
            "runtime aggregate child location form is unsupported".into(),
        ))
    })?;
    let pieces = evaluate_with_object(
        expression,
        endian,
        &mut FrameBase::Unsupported,
        units,
        runtime,
        budget,
        Some(object_address),
    )
    .map_err(|error| match error {
        EvaluateError::Unavailable(reason) => PathEvaluationError::Unavailable(reason),
        EvaluateError::Malformed(description) => PathEvaluationError::Malformed(description),
    })?;
    let [piece] = pieces.as_slice() else {
        return Err(PathEvaluationError::Malformed(
            "runtime aggregate child location produced multiple pieces".into(),
        ));
    };
    if piece.size_in_bits.is_some() || piece.bit_offset.is_some() {
        return Err(crate::UnsupportedVariableFeature::CompositeLocation.into());
    }
    let Location::Address { address } = piece.location else {
        return Err(PathEvaluationError::Malformed(
            "runtime aggregate child location did not produce an address".into(),
        ));
    };
    Ok(VirtualAddress::new(address))
}

#[expect(
    clippy::too_many_lines,
    reason = "gimli evaluation requirements are exhaustively and explicitly resumed in one loop"
)]
fn evaluate_with_object<'expression>(
    expression: &'expression Expression,
    endian: RunTimeEndian,
    frame_base: &mut FrameBase<'_>,
    units: &[EvaluationUnit],
    runtime: &mut dyn VariableRuntime,
    budget: &mut EvaluationBudget,
    object_address: Option<VirtualAddress>,
) -> std::result::Result<Vec<gimli::Piece<Reader<'expression>>>, EvaluateError> {
    let reader = gimli::EndianSlice::new(&expression.bytes, endian);
    let mut evaluation = gimli::Expression(reader).evaluation(expression.encoding);
    if let Some(address) = object_address {
        evaluation.set_initial_value(address.get());
        evaluation.set_object_address(address.get());
    }
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
        BaseTypeEncoding::Boolean => {
            match decode_integer_value(type_info, bytes, target.byte_order)
                .map_err(VariableUnavailableReason::Other)?
            {
                IntegerValue::Unsigned(0) => Ok(ScalarValue::Boolean(false)),
                IntegerValue::Unsigned(1) => Ok(ScalarValue::Boolean(true)),
                IntegerValue::Unsigned(value) => {
                    Err(format!("invalid boolean representation {value}").into())
                }
                IntegerValue::Signed(_) => {
                    unreachable!("boolean encoding returns an unsigned value")
                }
            }
        }
        BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter => {
            let IntegerValue::Signed(value) =
                decode_integer_value(type_info, bytes, target.byte_order)
                    .map_err(VariableUnavailableReason::Other)?
            else {
                unreachable!("signed encoding returns a signed integer");
            };
            Ok(ScalarValue::Signed(value))
        }
        BaseTypeEncoding::Unsigned | BaseTypeEncoding::UnsignedCharacter => {
            let IntegerValue::Unsigned(value) =
                decode_integer_value(type_info, bytes, target.byte_order)
                    .map_err(VariableUnavailableReason::Other)?
            else {
                unreachable!("unsigned encoding returns an unsigned integer");
            };
            Ok(ScalarValue::Unsigned(value))
        }
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

    use gimli::write::{
        AttributeValue as WriteAttributeValue, Dwarf as WriteDwarf, EndianVec, LineProgram,
        Sections, Unit,
    };
    use gimli::{Encoding, Format, LittleEndian};

    #[test]
    fn declaration_canonicalization_cannot_launder_a_non_type_reference() {
        let encoding = Encoding {
            format: Format::Dwarf32,
            version: 5,
            address_size: 8,
        };
        let mut written = WriteDwarf::new();
        let unit_id = written.units.add(Unit::new(encoding, LineProgram::none()));
        let unit = written.units.get_mut(unit_id);
        let root = unit.root();
        let non_type = unit.add(root, gimli::DW_TAG_variable);
        let definition = unit.add(root, gimli::DW_TAG_base_type);
        unit.get_mut(definition).set(
            gimli::DW_AT_specification,
            WriteAttributeValue::UnitRef(non_type),
        );
        unit.get_mut(definition)
            .set(gimli::DW_AT_encoding, WriteAttributeValue::Data1(5));
        unit.get_mut(definition)
            .set(gimli::DW_AT_byte_size, WriteAttributeValue::Udata(4));
        let variable = unit.add(root, gimli::DW_TAG_variable);
        unit.get_mut(variable)
            .set(gimli::DW_AT_type, WriteAttributeValue::UnitRef(non_type));

        let mut sections = Sections::new(EndianVec::new(LittleEndian));
        written.write(&mut sections).expect("write test DWARF");
        let dwarf = gimli::Dwarf::load(|id| {
            let bytes = sections.get(id).map(EndianVec::slice).unwrap_or_default();
            Ok::<_, gimli::Error>(Reader::new(bytes, RunTimeEndian::Little))
        })
        .expect("read test DWARF");
        let mut headers = dwarf.units();
        let header = headers
            .next()
            .expect("read unit header")
            .expect("one test unit");
        let units = vec![dwarf.unit(header).expect("read test unit")];
        let type_value = {
            let mut entries = units[0].entries();
            let mut value = None;
            while let Some(entry) = entries.next_dfs().expect("read test DIE") {
                if entry.tag() == gimli::DW_TAG_variable
                    && let Some(candidate) = entry.attr_value(gimli::DW_AT_type)
                {
                    value = Some(candidate);
                }
            }
            value.expect("referencing variable")
        };
        let signatures = HashMap::new();
        let mut arena = TypeArenaBuilder::new(
            &dwarf,
            &units,
            &signatures,
            ModuleImageId::new(0),
            ByteOrder::Little,
        );

        assert!(matches!(
            arena.variable_type(0, Some(type_value)),
            TypeResolution::Malformed(description)
                if description.contains("non-type tag")
        ));
    }

    #[test]
    fn discriminant_leb128_parsers_cover_full_width_and_reject_overflow() {
        let mut unsigned_max = vec![0xff; 18];
        unsigned_max.push(0x03);
        let mut cursor = 0;
        assert_eq!(
            read_uleb128_u128(&unsigned_max, &mut cursor).unwrap(),
            u128::MAX
        );
        assert_eq!(cursor, unsigned_max.len());

        let mut unsigned_overflow = vec![0xff; 18];
        unsigned_overflow.push(0x04);
        assert!(read_uleb128_u128(&unsigned_overflow, &mut 0).is_err());
        assert_eq!(read_sleb128_i128(&[0x7d], &mut 0).unwrap(), -3);
        assert!(read_sleb128_i128(&[0x80; 20], &mut 0).is_err());
    }

    #[test]
    fn variant_selection_validation_rejects_overlap_and_multiple_defaults() {
        let variant = |selection| Variant {
            name: None,
            selection,
            members: Arc::from([]),
        };
        let overlapping = [
            variant(VariantSelection::Selectors(Arc::from([
                VariantSelector::Range {
                    low: IntegerValue::Signed(-3),
                    high: IntegerValue::Signed(3),
                },
            ]))),
            variant(VariantSelection::Selectors(Arc::from([
                VariantSelector::Value(IntegerValue::Signed(3)),
            ]))),
        ];
        assert!(validate_variant_selections(&overlapping).is_err());
        assert!(
            validate_variant_selections(&[
                variant(VariantSelection::Default),
                variant(VariantSelection::Default),
            ])
            .is_err()
        );
    }

    #[test]
    fn explicit_discriminant_lists_are_nonempty_and_share_one_metadata_budget() {
        let representation = scalar_type(BaseTypeEncoding::Unsigned, 1);
        let mut budget = VariantMetadataBudget::default();
        assert!(matches!(
            parse_discriminant_list(&[], &representation, &mut budget),
            Err(VariantMetadataError::Malformed(_))
        ));

        for _ in 0..MAX_VARIANT_METADATA - 1 {
            budget.consume().expect("budget entry");
        }
        let selector = [gimli::DW_DSC_label.0, 1];
        assert!(parse_discriminant_list(&selector, &representation, &mut budget).is_ok());
        assert_eq!(
            parse_discriminant_list(&selector, &representation, &mut budget),
            Err(VariantMetadataError::Limit)
        );
    }

    #[test]
    fn zig_synthetic_variant_names_require_canonical_type_syntax() {
        assert_eq!(zig_optional_payload_name("?u32"), Some("u32"));
        assert_eq!(zig_optional_payload_name("named.?u32"), None);
        assert_eq!(
            zig_error_union_type_names("error{bad}!u32"),
            Some(("error{bad}", "u32"))
        );
        assert_eq!(
            zig_error_union_type_names("anyerror!*const u8"),
            Some(("anyerror", "*const u8"))
        );
        assert_eq!(zig_error_union_type_names("user!record"), None);
        assert_eq!(zig_error_union_type_names("error{bad}!"), None);
    }

    #[test]
    fn sub_byte_integer_representations_mask_and_sign_extend() {
        let base = |encoding, bit_size| BaseType {
            name: "bits".into(),
            base_name: "bits".into(),
            encoding,
            byte_size: 1,
            bit_size: Some(bit_size),
        };
        assert_eq!(
            decode_integer_value(
                &base(BaseTypeEncoding::Unsigned, 3),
                &[0xff],
                ByteOrder::Little
            )
            .unwrap(),
            IntegerValue::Unsigned(7)
        );
        assert_eq!(
            decode_integer_value(
                &base(BaseTypeEncoding::Signed, 3),
                &[0x05],
                ByteOrder::Little
            )
            .unwrap(),
            IntegerValue::Signed(-3)
        );
    }

    #[test]
    fn enum_typed_record_bit_fields_retain_symbolic_values() {
        let image = ModuleImageId::new(1);
        let reference = |id| TypeReference {
            image,
            id: TypeId::new(id),
        };
        let representation = BaseType {
            name: "State".into(),
            base_name: "unsigned int".into(),
            encoding: BaseTypeEncoding::Unsigned,
            byte_size: 1,
            bit_size: None,
        };
        let enumerator = Enumerator {
            name: "Ready".into(),
            value: IntegerValue::Unsigned(1),
        };
        let types = [
            TypeEntry::Resolved(TypeInfo {
                reference: reference(0),
                name: "State".into(),
                byte_size: Some(1),
                kind: TypeKind::Enumeration {
                    representation,
                    underlying: None,
                    enumerators: Arc::from([enumerator.clone()]),
                    origin: EnumerationOrigin::Language,
                    scoped: false,
                },
            }),
            TypeEntry::Resolved(TypeInfo {
                reference: reference(1),
                name: "Packed".into(),
                byte_size: Some(1),
                kind: TypeKind::Record {
                    kind: RecordKind::Struct,
                    members: Arc::from([RecordMember {
                        name: Some("state".into()),
                        type_ref: reference(0),
                        layout: RecordMemberLayout::BitRange {
                            bit_offset: 0,
                            bit_size: 1,
                        },
                        accessibility: Accessibility::Public,
                        artificial: false,
                        embedded: false,
                        declaration: None,
                    }]),
                    bases: Arc::from([]),
                    incomplete: false,
                },
            }),
        ];
        let shape = value_shape_from(&types, TypeId::new(1)).expect("record shape");
        let graph =
            decode_value_graph(&types, &shape, Arc::from([1_u8]), target(ByteOrder::Little))
                .expect("record graph");
        let ValueNodeState::Available(VariableValue::Record { members, .. }) = &graph.root().state
        else {
            panic!("root was not a record: {graph:?}");
        };
        assert!(matches!(
            graph.node(members[0].value).map(|node| &node.state),
            Some(ValueNodeState::Available(VariableValue::Enumeration {
                value: IntegerValue::Unsigned(1),
                matches,
            })) if matches.as_ref() == [enumerator]
        ));
    }

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
            bit_size: None,
        }
    }

    fn indirection_shape(address_class: u64) -> ValueShape {
        let reference = TypeReference {
            image: ModuleImageId::new(1),
            id: TypeId::new(0),
        };
        ValueShape {
            type_info: TypeInfo {
                reference,
                name: "test *".into(),
                byte_size: Some(8),
                kind: TypeKind::Pointer {
                    target: Some(TypeReference {
                        image: reference.image,
                        id: TypeId::new(1),
                    }),
                    address_class,
                },
            },
            kind: ValueShapeKind::Indirection {
                target: Some(TypeId::new(1)),
                byte_size: 8,
                address_class,
            },
        }
    }

    fn slice_types(has_capacity: bool) -> Vec<TypeEntry> {
        let byte_size = if has_capacity { 24 } else { 16 };
        let image = ModuleImageId::new(1);
        let element = TypeReference {
            image,
            id: TypeId::new(0),
        };
        vec![
            TypeEntry::Resolved(TypeInfo {
                reference: element,
                name: "test".into(),
                byte_size: Some(4),
                kind: TypeKind::Base(scalar_type(BaseTypeEncoding::Signed, 4)),
            }),
            TypeEntry::Resolved(TypeInfo {
                reference: TypeReference {
                    image,
                    id: TypeId::new(1),
                },
                name: "test slice".into(),
                byte_size: Some(byte_size),
                kind: TypeKind::Slice {
                    element,
                    has_capacity,
                },
            }),
        ]
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
    fn static_member_layouts_cannot_escape_their_containing_record() {
        assert!(static_member_layout_is_valid(
            Some(8),
            Some(4),
            RecordMemberLayout::ByteOffset(4)
        ));
        assert!(!static_member_layout_is_valid(
            Some(8),
            Some(4),
            RecordMemberLayout::ByteOffset(5)
        ));
        assert!(!static_member_layout_is_valid(
            Some(u64::MAX),
            Some(2),
            RecordMemberLayout::ByteOffset(u64::MAX)
        ));
        assert!(static_member_layout_is_valid(
            Some(8),
            Some(1),
            RecordMemberLayout::BitRange {
                bit_offset: 63,
                bit_size: 1,
            }
        ));
        assert!(!static_member_layout_is_valid(
            Some(8),
            Some(1),
            RecordMemberLayout::BitRange {
                bit_offset: 63,
                bit_size: 2,
            }
        ));
        assert!(!static_member_layout_is_valid(
            None,
            Some(1),
            RecordMemberLayout::ByteOffset(0)
        ));
        assert!(static_member_layout_is_valid(
            None,
            Some(1),
            RecordMemberLayout::Runtime
        ));
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
                kind: TypeKind::Named {
                    target: Some(reference(1)),
                    relationship: NamedTypeRelationship::Synonym,
                },
            }),
            TypeEntry::Resolved(TypeInfo {
                reference: reference(1),
                name: "right".into(),
                byte_size: Some(8),
                kind: TypeKind::Modified {
                    modifier: TypeModifier::Const,
                    target: reference(0),
                },
            }),
        ];
        let cycle_error = value_shape_from(&cycle, TypeId::new(0)).unwrap_err();
        assert!(
            matches!(&cycle_error, ValueShapeError::Malformed(description) if description.as_ref() == "type wrapper cycle"),
            "wrapper cycle must classify as malformed metadata",
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
            Ok(ValueShape { kind: ValueShapeKind::Indirection {
                target: Some(id),
                byte_size: 8,
                address_class: 0,
            }, .. }) if id == TypeId::new(0)
        ));
    }

    #[test]
    fn graph_finalization_propagates_wrapper_sizes_to_a_fixpoint() {
        let image = ModuleImageId::new(7);
        let reference = |id| TypeReference {
            image,
            id: TypeId::new(id),
        };
        let mut types = [
            TypeEntry::Resolved(TypeInfo {
                reference: reference(0),
                name: "outer".into(),
                byte_size: None,
                kind: TypeKind::Named {
                    target: Some(reference(1)),
                    relationship: NamedTypeRelationship::Synonym,
                },
            }),
            TypeEntry::Resolved(TypeInfo {
                reference: reference(1),
                name: "const inner".into(),
                byte_size: None,
                kind: TypeKind::Modified {
                    modifier: TypeModifier::Const,
                    target: reference(2),
                },
            }),
            TypeEntry::Resolved(TypeInfo {
                reference: reference(2),
                name: "inner".into(),
                byte_size: Some(8),
                kind: TypeKind::Opaque {
                    description: "test representation".into(),
                },
            }),
        ];

        propagate_wrapper_sizes(&mut types);

        assert!(
            types.iter().all(
                |entry| matches!(entry, TypeEntry::Resolved(info) if info.byte_size == Some(8))
            ),
            "{types:?}"
        );
    }

    #[test]
    fn named_relationships_and_modifier_tags_are_explicit() {
        assert_eq!(
            named_type_relationship(gimli::DW_TAG_typedef, Some(gimli::DW_LANG_C11), false),
            NamedTypeRelationship::Synonym
        );
        assert_eq!(
            named_type_relationship(
                gimli::DW_TAG_template_alias,
                Some(gimli::DW_LANG_Rust),
                false,
            ),
            NamedTypeRelationship::Synonym
        );
        assert_eq!(
            named_type_relationship(gimli::DW_TAG_typedef, Some(gimli::DW_LANG_Go), false),
            NamedTypeRelationship::Distinct
        );
        assert_eq!(
            named_type_relationship(gimli::DW_TAG_typedef, Some(gimli::DW_LANG_C99), true),
            NamedTypeRelationship::Encoding
        );
        assert_eq!(
            named_type_relationship(gimli::DW_TAG_typedef, Some(gimli::DW_LANG_Rust), false),
            NamedTypeRelationship::Unspecified
        );
        assert_eq!(
            named_type_target_requirement(true),
            TargetRequirement::Optional
        );
        assert_eq!(
            named_type_target_requirement(false),
            TargetRequirement::Required
        );
        assert_eq!(
            type_modifier(gimli::DW_TAG_packed_type),
            Some(TypeModifier::Packed)
        );
        assert_eq!(
            type_modifier(gimli::DW_TAG_shared_type),
            Some(TypeModifier::Shared)
        );
        assert_eq!(type_modifier(gimli::DW_TAG_typedef), None);
        assert_eq!(
            modifier_type_name(TypeModifier::Const, "int * volatile", true),
            "int * volatile const"
        );
        assert_eq!(
            modifier_type_name(TypeModifier::Const, "int", false),
            "const int"
        );
        assert_eq!(
            modifier_type_name(TypeModifier::Atomic, "int", false),
            "_Atomic(int)"
        );
    }

    #[test]
    fn inline_cycle_analysis_distinguishes_storage_from_indirection() {
        let image = ModuleImageId::new(7);
        let reference = |id| TypeReference {
            image,
            id: TypeId::new(id),
        };
        let by_value_cycle = [
            TypeEntry::Resolved(TypeInfo {
                reference: reference(0),
                name: "left".into(),
                byte_size: Some(8),
                kind: TypeKind::Named {
                    target: Some(reference(1)),
                    relationship: NamedTypeRelationship::Synonym,
                },
            }),
            TypeEntry::Resolved(TypeInfo {
                reference: reference(1),
                name: "right".into(),
                byte_size: Some(8),
                kind: TypeKind::Modified {
                    modifier: TypeModifier::Const,
                    target: reference(0),
                },
            }),
        ];
        let mut cycle_nodes = inline_storage_cycle_nodes(&by_value_cycle);
        cycle_nodes.sort_unstable();
        assert_eq!(cycle_nodes, [0, 1]);

        let pointer_recursion = [
            TypeEntry::Resolved(TypeInfo {
                reference: reference(0),
                name: "node".into(),
                byte_size: Some(8),
                kind: TypeKind::Record {
                    kind: RecordKind::Struct,
                    members: Arc::from([RecordMember {
                        name: Some("next".into()),
                        type_ref: reference(1),
                        layout: RecordMemberLayout::ByteOffset(0),
                        accessibility: Accessibility::Public,
                        artificial: false,
                        embedded: false,
                        declaration: None,
                    }]),
                    bases: Arc::default(),
                    incomplete: false,
                },
            }),
            TypeEntry::Resolved(TypeInfo {
                reference: reference(1),
                name: "node *".into(),
                byte_size: Some(8),
                kind: TypeKind::Pointer {
                    target: Some(reference(0)),
                    address_class: 0,
                },
            }),
        ];
        assert!(inline_storage_cycle_nodes(&pointer_recursion).is_empty());
    }

    #[test]
    fn transparent_wrappers_reject_incompatible_storage_sizes() {
        let image = ModuleImageId::new(7);
        let reference = |id| TypeReference {
            image,
            id: TypeId::new(id),
        };
        let types = [
            TypeEntry::Resolved(TypeInfo {
                reference: reference(0),
                name: "encoded".into(),
                byte_size: Some(4),
                kind: TypeKind::Named {
                    target: Some(reference(1)),
                    relationship: NamedTypeRelationship::Encoding,
                },
            }),
            TypeEntry::Resolved(TypeInfo {
                reference: reference(1),
                name: "representation".into(),
                byte_size: Some(8),
                kind: TypeKind::Opaque {
                    description: "test representation".into(),
                },
            }),
        ];

        assert!(matches!(
            value_shape_from(&types, TypeId::new(0)),
            Err(ValueShapeError::Unsupported(description))
                if description.contains("wrapper size 4 differs from target size 8")
        ));

        let malformed_target = [
            types[0].clone(),
            TypeEntry::Malformed("broken representation".into()),
        ];
        assert!(matches!(
            value_shape_from(&malformed_target, TypeId::new(0)),
            Err(ValueShapeError::Malformed(description))
                if description.as_ref() == "broken representation"
        ));

        let shared = [
            TypeEntry::Resolved(TypeInfo {
                reference: reference(0),
                name: "shared representation".into(),
                byte_size: Some(8),
                kind: TypeKind::Modified {
                    modifier: TypeModifier::Shared,
                    target: reference(1),
                },
            }),
            types[1].clone(),
        ];
        assert!(matches!(
            value_shape_from(&shared, TypeId::new(0)),
            Err(ValueShapeError::Unsupported(description))
                if description.contains("distributed-memory semantics")
        ));
        let malformed_shared_target = [
            shared[0].clone(),
            TypeEntry::Malformed("broken shared representation".into()),
        ];
        assert!(matches!(
            value_shape_from(&malformed_shared_target, TypeId::new(0)),
            Err(ValueShapeError::Malformed(description))
                if description.as_ref() == "broken shared representation"
        ));
    }

    #[test]
    fn sizeless_pointers_separate_unsupported_address_classes_from_defective_metadata() {
        let reference = |id| TypeReference {
            image: ModuleImageId::new(7),
            id: TypeId::new(id),
        };
        let sizeless = |address_class| {
            [TypeEntry::Resolved(TypeInfo {
                reference: reference(0),
                name: "opaque *".into(),
                byte_size: None,
                kind: TypeKind::Pointer {
                    target: Some(reference(0)),
                    address_class,
                },
            })]
        };

        // A non-default address class the backend cannot size is valid but
        // unsupported metadata, not a defect.
        let unsupported = value_shape_from(&sizeless(2), TypeId::new(0)).unwrap_err();
        assert!(
            matches!(&unsupported, ValueShapeError::Unsupported(description)
                if description.contains("address class 2")),
            "non-default address class without a size must be unsupported: {unsupported:?}",
        );

        // A missing size under the default address class is an internal
        // inconsistency the builder never emits, so it stays malformed.
        let malformed = value_shape_from(&sizeless(0), TypeId::new(0)).unwrap_err();
        assert!(
            matches!(&malformed, ValueShapeError::Malformed(description)
                if description.as_ref() == "pointer type has no byte size"),
            "default address class without a size must be malformed: {malformed:?}",
        );

        // A zero-byte indirection cannot hold an address, so it is defective
        // rather than a usable shape that would later read zero bytes.
        let zero_sized = [TypeEntry::Resolved(TypeInfo {
            reference: reference(0),
            name: "opaque *".into(),
            byte_size: Some(0),
            kind: TypeKind::Pointer {
                target: Some(reference(0)),
                address_class: 0,
            },
        })];
        let zero_error = value_shape_from(&zero_sized, TypeId::new(0)).unwrap_err();
        assert!(
            matches!(&zero_error, ValueShapeError::Malformed(description)
                if description.as_ref() == "pointer type has a zero byte size"),
            "zero-sized pointer must be malformed: {zero_error:?}",
        );

        // A scalar encoding likewise cannot occupy zero bytes.
        let zero_scalar = [TypeEntry::Resolved(TypeInfo {
            reference: reference(0),
            name: "empty".into(),
            byte_size: Some(0),
            kind: TypeKind::Base(scalar_type(BaseTypeEncoding::Unsigned, 0)),
        })];
        let zero_scalar_error = value_shape_from(&zero_scalar, TypeId::new(0)).unwrap_err();
        assert!(
            matches!(&zero_scalar_error, ValueShapeError::Malformed(description)
                if description.as_ref() == "base type has a zero byte size"),
            "zero-sized base type must be malformed: {zero_scalar_error:?}",
        );

        // A width wider than a decodable address is valid metadata this backend
        // cannot use, so it is unsupported rather than a usable shape that would
        // drive a doomed inferior read.
        let oversized = [TypeEntry::Resolved(TypeInfo {
            reference: reference(0),
            name: "wide *".into(),
            byte_size: Some(16),
            kind: TypeKind::Pointer {
                target: Some(reference(0)),
                address_class: 0,
            },
        })];
        let oversized_error = value_shape_from(&oversized, TypeId::new(0)).unwrap_err();
        assert!(
            matches!(&oversized_error, ValueShapeError::Unsupported(description)
                if description.contains("wider than")),
            "over-wide pointer must be unsupported: {oversized_error:?}",
        );
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
        let shape = indirection_shape;
        let mut runtime = Runtime {
            registers: BTreeMap::new(),
            cfa: Err(VariableUnavailableReason::CfaExpression),
            memory: None,
            memory_reads: 0,
        };
        assert!(matches!(
            decode_value_state(
                &[],
                &HashMap::new(),
                &[],
                RunTimeEndian::Little,
                &shape(0),
                context,
                VariableValueSource::Computed,
                Arc::from([0_u8; 8]),
                target(ByteOrder::Little),
                &mut runtime,
                EvaluationBudget::default(),
            ),
            VariableState::Available {
                dereference: DereferenceState::Unavailable {
                    reason: DereferenceUnavailableReason::Null,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            decode_value_state(
                &[],
                &HashMap::new(),
                &[],
                RunTimeEndian::Little,
                &shape(17),
                context,
                VariableValueSource::Computed,
                Arc::from(1_u64.to_le_bytes()),
                target(ByteOrder::Little),
                &mut runtime,
                EvaluationBudget::default(),
            ),
            VariableState::Available {
                dereference: DereferenceState::Unavailable {
                    reason: DereferenceUnavailableReason::AddressClass(17),
                    ..
                },
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

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one table-like test exercises every slice descriptor and shared-budget boundary"
    )]
    fn slice_decoding_bounds_secondary_reads_and_validates_descriptors() {
        let target = target(ByteOrder::Little);
        let descriptor = |address: u64, length: u64, capacity: Option<u64>| {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&address.to_le_bytes());
            bytes.extend_from_slice(&length.to_le_bytes());
            if let Some(capacity) = capacity {
                bytes.extend_from_slice(&capacity.to_le_bytes());
            }
            Arc::from(bytes)
        };
        let runtime = |memory: Option<Arc<[u8]>>| Runtime {
            registers: BTreeMap::new(),
            cfa: Err(VariableUnavailableReason::CfaExpression),
            memory,
            memory_reads: 0,
        };

        let mut rust = runtime(Some(Arc::from(
            [20_i32.to_le_bytes(), 22_i32.to_le_bytes()].concat(),
        )));
        let rust_types = slice_types(false);
        let rust_shape = value_shape_from(&rust_types, TypeId::new(1)).expect("slice shape");
        let state = decode_slice_state(
            &rust_types,
            &HashMap::new(),
            &[],
            RunTimeEndian::Little,
            &rust_shape,
            VariableValueSource::Computed,
            descriptor(0x1000, 2, None),
            target,
            &mut rust,
            EvaluationBudget::default(),
        );
        assert!(matches!(state, VariableState::Available { ref value, .. }
            if matches!(&value.root().state,
                ValueNodeState::Available(VariableValue::Slice {
                    length: 2, capacity: None, elements, ..
                }) if elements.len() == 2)));
        assert_eq!(rust.memory_reads, 1);

        let mut empty = runtime(None);
        let go_types = slice_types(true);
        let go_shape = value_shape_from(&go_types, TypeId::new(1)).expect("slice shape");
        assert!(matches!(
            decode_slice_state(
                &go_types,
                &HashMap::new(),
                &[],
                RunTimeEndian::Little,
                &go_shape,
                VariableValueSource::Computed,
                descriptor(0, 0, Some(0)),
                target,
                &mut empty,
                EvaluationBudget::default(),
            ),
            VariableState::Available { ref value, .. }
                if matches!(&value.root().state,
                    ValueNodeState::Available(VariableValue::Slice {
                        length: 0, capacity: Some(0), elements, ..
                    }) if elements.is_empty())
        ));
        assert_eq!(empty.memory_reads, 0);

        let mut invalid = runtime(None);
        assert!(matches!(
            decode_slice_state(
                &go_types,
                &HashMap::new(),
                &[],
                RunTimeEndian::Little,
                &go_shape,
                VariableValueSource::Computed,
                descriptor(0x1000, 3, Some(2)),
                target,
                &mut invalid,
                EvaluationBudget::default(),
            ),
            VariableState::Malformed(_)
        ));
        assert_eq!(invalid.memory_reads, 0);

        let mut oversized = runtime(None);
        assert!(matches!(
            decode_slice_state(
                &rust_types,
                &HashMap::new(),
                &[],
                RunTimeEndian::Little,
                &rust_shape,
                VariableValueSource::Computed,
                descriptor(0x1000, 257, None),
                target,
                &mut oversized,
                EvaluationBudget::default(),
            ),
            VariableState::Unavailable(VariableUnavailableReason::EvaluationLimit)
        ));
        assert_eq!(oversized.memory_reads, 0);

        let mut exhausted = runtime(Some(Arc::from([0_u8; 8])));
        assert!(matches!(
            decode_slice_state(
                &rust_types,
                &HashMap::new(),
                &[],
                RunTimeEndian::Little,
                &rust_shape,
                VariableValueSource::Computed,
                descriptor(0x1000, 2, None),
                target,
                &mut exhausted,
                EvaluationBudget {
                    memory_reads: 1,
                    memory_bytes: MAX_EVALUATION_MEMORY_BYTES - 4,
                },
            ),
            VariableState::Unavailable(VariableUnavailableReason::EvaluationLimit)
        ));
        assert_eq!(exhausted.memory_reads, 0);
    }

    #[test]
    fn bit_field_extraction_is_endian_aware_and_bounded() {
        assert_eq!(
            extract_bit_field(&[0b1010_1101], 0, 3, ByteOrder::Little).expect("little field"),
            0b101
        );
        assert_eq!(
            extract_bit_field(&[0b1010_1101], 0, 3, ByteOrder::Big).expect("big field"),
            0b101
        );
        assert_eq!(
            extract_bit_field(&[0b1010_1101], 3, 5, ByteOrder::Little).expect("little tail"),
            0b10101
        );
        assert_eq!(
            extract_bit_field(&[0b1010_1101], 3, 5, ByteOrder::Big).expect("big tail"),
            0b01101
        );
        assert!(extract_bit_field(&[0], 7, 2, ByteOrder::Little).is_err());
        assert!(extract_bit_field(&[0], 0, 0, ByteOrder::Little).is_err());
    }

    #[test]
    fn record_graphs_decode_nested_values_and_preserve_partial_failures() {
        let image = ModuleImageId::new(1);
        let reference = |id| TypeReference {
            image,
            id: TypeId::new(id),
        };
        let member = |name: &'static str, offset| RecordMember {
            name: Some(name.into()),
            type_ref: reference(0),
            layout: RecordMemberLayout::ByteOffset(offset),
            accessibility: Accessibility::Public,
            artificial: false,
            embedded: false,
            declaration: None,
        };
        let types = vec![
            TypeEntry::Resolved(TypeInfo {
                reference: reference(0),
                name: "i32".into(),
                byte_size: Some(4),
                kind: TypeKind::Base(scalar_type(BaseTypeEncoding::Signed, 4)),
            }),
            TypeEntry::Resolved(TypeInfo {
                reference: reference(1),
                name: "Pair".into(),
                byte_size: Some(8),
                kind: TypeKind::Record {
                    kind: RecordKind::Struct,
                    members: Arc::from([member("first", 0), member("second", 4)]),
                    bases: Arc::from([]),
                    incomplete: false,
                },
            }),
            TypeEntry::Resolved(TypeInfo {
                reference: reference(2),
                name: "Broken".into(),
                byte_size: Some(8),
                kind: TypeKind::Record {
                    kind: RecordKind::Struct,
                    members: Arc::from([member("valid", 0), member("outside", 8)]),
                    bases: Arc::from([]),
                    incomplete: false,
                },
            }),
        ];
        let pair = value_shape_from(&types, TypeId::new(1)).expect("pair shape");
        let graph = decode_value_graph(
            &types,
            &pair,
            Arc::from([20_i32.to_le_bytes(), 22_i32.to_le_bytes()].concat()),
            target(ByteOrder::Little),
        )
        .expect("pair graph");
        let ValueNodeState::Available(VariableValue::Record { members, .. }) = &graph.root().state
        else {
            panic!("pair root was not a record: {graph:?}");
        };
        assert_eq!(members.len(), 2);
        assert!(matches!(
            graph.node(members[0].value).map(|node| &node.state),
            Some(ValueNodeState::Available(VariableValue::Scalar(
                ScalarValue::Signed(20)
            )))
        ));
        assert!(matches!(
            graph.node(members[1].value).map(|node| &node.state),
            Some(ValueNodeState::Available(VariableValue::Scalar(
                ScalarValue::Signed(22)
            )))
        ));

        let broken = value_shape_from(&types, TypeId::new(2)).expect("broken shape");
        let graph = decode_value_graph(
            &types,
            &broken,
            Arc::from([42_u8; 8]),
            target(ByteOrder::Little),
        )
        .expect("partial graph");
        let ValueNodeState::Available(VariableValue::Record { members, .. }) = &graph.root().state
        else {
            panic!("broken root was not a record: {graph:?}");
        };
        assert!(matches!(
            graph.node(members[0].value).map(|node| &node.state),
            Some(ValueNodeState::Available(_))
        ));
        assert!(matches!(
            graph.node(members[1].value).map(|node| &node.state),
            Some(ValueNodeState::Malformed(_))
        ));
    }

    #[test]
    fn aggregate_node_limit_reports_exact_omissions_without_partial_ids() {
        let image = ModuleImageId::new(1);
        let reference = |id| TypeReference {
            image,
            id: TypeId::new(id),
        };
        let count = u64::try_from(MAX_VALUE_NODES + 5).expect("test count fits u64");
        let types = [
            TypeEntry::Resolved(TypeInfo {
                reference: reference(0),
                name: "u8".into(),
                byte_size: Some(1),
                kind: TypeKind::Base(scalar_type(BaseTypeEncoding::Unsigned, 1)),
            }),
            TypeEntry::Resolved(TypeInfo {
                reference: reference(1),
                name: "bounded".into(),
                byte_size: Some(count),
                kind: TypeKind::Array {
                    element: reference(0),
                    dimensions: Arc::from([ArrayDimension {
                        lower_bound: 0,
                        count,
                    }]),
                },
            }),
        ];
        let shape = value_shape_from(&types, TypeId::new(1)).expect("array shape");
        let graph = decode_value_graph(
            &types,
            &shape,
            Arc::from(vec![
                7_u8;
                usize::try_from(count).expect("test count fits usize")
            ]),
            target(ByteOrder::Little),
        )
        .expect("bounded graph");
        assert_eq!(graph.nodes().len(), MAX_VALUE_NODES);
        assert!(matches!(
            &graph.root().state,
            ValueNodeState::Available(VariableValue::Array { elements, omitted, .. })
                if elements.len() == MAX_VALUE_NODES - 1 && *omitted == 6
        ));
    }

    #[test]
    fn direct_by_value_record_cycles_are_malformed_without_recursing() {
        let reference = TypeReference {
            image: ModuleImageId::new(1),
            id: TypeId::new(0),
        };
        let types = [TypeEntry::Resolved(TypeInfo {
            reference,
            name: "Impossible".into(),
            byte_size: Some(1),
            kind: TypeKind::Record {
                kind: RecordKind::Struct,
                members: Arc::from([RecordMember {
                    name: Some("self".into()),
                    type_ref: reference,
                    layout: RecordMemberLayout::ByteOffset(0),
                    accessibility: Accessibility::Public,
                    artificial: false,
                    embedded: false,
                    declaration: None,
                }]),
                bases: Arc::from([]),
                incomplete: false,
            },
        })];
        let shape = value_shape_from(&types, TypeId::new(0)).expect("record shape");
        let graph =
            decode_value_graph(&types, &shape, Arc::from([0_u8]), target(ByteOrder::Little))
                .expect("bounded malformed graph");
        let ValueNodeState::Available(VariableValue::Record { members, .. }) = &graph.root().state
        else {
            panic!("root was not a partial record: {graph:?}");
        };
        assert!(matches!(
            graph.node(members[0].value).map(|node| &node.state),
            Some(ValueNodeState::Malformed(_))
        ));
    }

    #[test]
    fn indirect_by_value_record_cycles_are_malformed_without_reaching_depth_limit() {
        let image = ModuleImageId::new(1);
        let reference = |id| TypeReference {
            image,
            id: TypeId::new(id),
        };
        let member = |name: &'static str, target| RecordMember {
            name: Some(name.into()),
            type_ref: reference(target),
            layout: RecordMemberLayout::ByteOffset(0),
            accessibility: Accessibility::Public,
            artificial: false,
            embedded: false,
            declaration: None,
        };
        let types = [
            TypeEntry::Resolved(TypeInfo {
                reference: reference(0),
                name: "A".into(),
                byte_size: Some(1),
                kind: TypeKind::Record {
                    kind: RecordKind::Struct,
                    members: Arc::from([member("b", 1)]),
                    bases: Arc::from([]),
                    incomplete: false,
                },
            }),
            TypeEntry::Resolved(TypeInfo {
                reference: reference(1),
                name: "B".into(),
                byte_size: Some(1),
                kind: TypeKind::Record {
                    kind: RecordKind::Struct,
                    members: Arc::from([member("a", 0)]),
                    bases: Arc::from([]),
                    incomplete: false,
                },
            }),
        ];
        let shape = value_shape_from(&types, TypeId::new(0)).expect("record shape");
        let graph =
            decode_value_graph(&types, &shape, Arc::from([0_u8]), target(ByteOrder::Little))
                .expect("bounded malformed graph");
        assert_eq!(graph.nodes().len(), 3);
        let ValueNodeState::Available(VariableValue::Record { members, .. }) = &graph.root().state
        else {
            panic!("root was not a record: {graph:?}");
        };
        let b = graph.node(members[0].value).expect("B node");
        let ValueNodeState::Available(VariableValue::Record { members, .. }) = &b.state else {
            panic!("B was not a record: {b:?}");
        };
        assert!(matches!(
            graph.node(members[0].value).map(|node| &node.state),
            Some(ValueNodeState::Malformed(reason))
                if reason.description.contains("by-value cycle")
        ));
    }

    #[test]
    fn zero_sized_records_and_arrays_remain_bounded_without_reading_storage() {
        let image = ModuleImageId::new(1);
        let reference = |id| TypeReference {
            image,
            id: TypeId::new(id),
        };
        let count = u64::try_from(MAX_VALUE_NODES + 7).expect("test count fits u64");
        let types = [
            TypeEntry::Resolved(TypeInfo {
                reference: reference(0),
                name: "Empty".into(),
                byte_size: Some(0),
                kind: TypeKind::Record {
                    kind: RecordKind::Struct,
                    members: Arc::from([]),
                    bases: Arc::from([]),
                    incomplete: false,
                },
            }),
            TypeEntry::Resolved(TypeInfo {
                reference: reference(1),
                name: "ManyEmpty".into(),
                byte_size: Some(0),
                kind: TypeKind::Array {
                    element: reference(0),
                    dimensions: Arc::from([ArrayDimension {
                        lower_bound: 0,
                        count,
                    }]),
                },
            }),
        ];
        let empty = value_shape_from(&types, TypeId::new(0)).expect("empty record shape");
        let graph = decode_value_graph(&types, &empty, Arc::from([]), target(ByteOrder::Little))
            .expect("empty record graph");
        assert!(matches!(
            &graph.root().state,
            ValueNodeState::Available(VariableValue::Record { members, bases, .. })
                if members.is_empty() && bases.is_empty()
        ));

        let array = value_shape_from(&types, TypeId::new(1)).expect("zero-sized array shape");
        let graph = decode_value_graph(&types, &array, Arc::from([]), target(ByteOrder::Little))
            .expect("bounded zero-sized array graph");
        assert_eq!(graph.nodes().len(), MAX_VALUE_NODES);
        assert!(matches!(
            &graph.root().state,
            ValueNodeState::Available(VariableValue::Array { elements, omitted, .. })
                if elements.len() == MAX_VALUE_NODES - 1 && *omitted == 8
        ));
    }
}
