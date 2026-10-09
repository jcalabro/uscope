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
//! - [`pieces`]: the storage location pieces describe; [`storage`]: reading
//!   and selecting storage of every form.
//! - [`call_sites`]: the calls that recover parameters' entry values.

use crate::image::lines::Files;
use std::collections::BTreeMap;
use std::sync::Arc;

use foldhash::{HashMap, HashMapExt};
use gimli::RunTimeEndian;

use crate::debug_info::{
    Accessed, CallSite, CallSiteId, EntryParameter, Located, ObjectKey, ObjectStorage, PlannedStep,
    Step, TextLocation, VariableContext, VariableInfo, VariableRuntime, VariableRuntimeError,
};
use crate::inspection::InspectionBudget;
use crate::{
    AddressRange, ByteOrder, CodeInstanceId, DereferenceReference, DereferencedValue, Error,
    GlobalVariableId, GlobalVariableInfo, ImageAddress, InspectedValue, ModuleImageId, Result,
    SourceFileId, SourceLanguage, SourceLocation, TargetDescription, TypeId, TypeInfo, TypeNode,
    TypeReference, ValueChildPage, ValueChildrenReference, Variable, VariableKind,
    VariableMalformedKind, VariableMalformedReason, VariableQuery, VariableState,
};

use super::{
    DieKey, DwarfError, Reader, UnitCatalog, Units, die_code_ranges, die_reference, is_type_unit,
};
use die::{
    check_data_object_capacity, copy_name, data_object_scope_ranges, debug_info_offset,
    declaration_with_origins, is_type_scope, origin_chain, strict_flag, string_with_origins,
    type_with_origins, variable_order_key,
};
use evaluate::FrameBaseCache;
use globals::{load_globals, public_global_type};
pub(in crate::debug_info) use identity::source_language;
pub(in crate::debug_info) use inspect::{PathStep, array_byte_offset};
use inspect::{data_object, evaluate_error_state, inspected_value};
use location::{
    EvaluationUnit, Expression, LocationDescription, copy_data_object_value,
    copy_optional_location, load_evaluation_units,
};
use types::{
    DynamicAggregateLayoutKey, TypeArenaBuilder, TypeEntry, TypeMetadataEntry, TypeResolution,
};

mod call_sites;
mod codec;
mod coroutine;
mod die;
mod evaluate;
mod generic;
mod globals;
mod identity;
mod inspect;
mod location;
mod pieces;
mod returns;
mod shape;
mod storage;
mod text;
mod types;
mod variant;
mod visibility;

const MAX_SCALAR_BYTES: u64 = 16;
const MAX_EVALUATION_ITERATIONS: u32 = 10_000;
const MAX_EVALUATION_MEMORY_BYTES: usize = 1_024;
const MAX_LOCATION_PIECES: usize = 64;
/// rustc describes a type again in every unit that uses it, so a program
/// built with tokio has some 70,000.
const MAX_TYPES: usize = 1 << 20;
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
    /// Whether the compiler made it for itself, such as Go's `.dict` and
    /// `#yield1`: listings leave it out, but its name still reaches it.
    hidden: bool,
    /// For `$future`, the future rustc passes the body of an async
    /// function or block, as a pointer it leaves unnamed: its type.
    coroutine: Option<TypeId>,
    value: Metadata<ValueDescription>,
    frame_base: Metadata<LocationDescription>,
    malformed: Option<Arc<str>>,
}

impl CatalogDataObject {
    /// Whether one location describes the object wherever it is in scope,
    /// rather than a list of locations by address.
    fn single_location(&self) -> bool {
        matches!(
            &self.value,
            Metadata::Value(ValueDescription::Location(location))
                if matches!(location.entries.as_ref(), [entry] if entry.range.is_none())
        )
    }
}

/// What kind of Rust scope a scope is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RustScope {
    /// The outermost scope of the body of an `async fn`, where rustc
    /// declares the fields of its future that captured its arguments, which
    /// the body moves into variables of its own.
    AsyncCaptures,
    Other,
}

impl RustScope {
    /// The scope of a subprogram or inline instance of a Rust unit.
    fn routine<'data>(
        dwarf: &gimli::Dwarf<Reader<'data>>,
        units: &Units<'data>,
        unit_index: usize,
        unit: &gimli::Unit<Reader<'data>>,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
    ) -> Self {
        if async_fn_body(dwarf, units, unit_index, unit, entry) {
            Self::AsyncCaptures
        } else {
            Self::Other
        }
    }
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
    /// The code instance whose code the scope is in: `instance`, or the
    /// physical frame's.
    code_instance: Option<CodeInstanceId>,
    /// The file declaring a Go function, and so its variables, whose
    /// declarations give only their line.
    go_file: Option<SourceFileId>,
    /// For a Rust scope, whose compiler names variables of its own, what
    /// kind it is.
    rust: Option<RustScope>,
    /// The line of the await whose future this scope, or one enclosing it,
    /// declares as `__awaitee`: rustc binds the value it gives as `result`
    /// in a block within.
    awaitee: Option<crate::LineNumber>,
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
    /// How the function returns its values, when that is known.
    returns: Option<returns::ReturnConvention>,
}

/// A `DW_TAG_dwarf_procedure`, which an implicit pointer may point into.
struct CatalogProcedure {
    location: Metadata<LocationDescription>,
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
    /// The async bodies' futures, by type.
    coroutines: BTreeMap<TypeId, crate::CoroutineInfo>,
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
    /// Whether each C++ class whose producer says how calls pass it is
    /// passed by value.
    passed_by_value: HashMap<TypeId, bool>,
    /// Go's type parameters: the dictionary entry each shape typedef names.
    go_dict_indices: HashMap<TypeId, u64>,
    /// The first type, in identifier order, each Go runtime type descriptor
    /// offset describes.
    go_runtime_types: HashMap<u64, TypeId>,
    objects_by_debug_offset: HashMap<u64, usize>,
    procedures: HashMap<u64, CatalogProcedure>,
    call_sites: call_sites::CallSiteCatalog,
    target: TargetDescription,
    endian: RunTimeEndian,
    /// For a variable of an async body, by its entry's offset, and each
    /// suspended state of the body's future: the code where the variable
    /// still holds what it held before the state's await, which execution
    /// reaches from where the state resumes without leaving the variable's
    /// scope.
    held: BTreeMap<(u64, u64), Arc<[crate::AddressRange<ImageAddress>]>>,
}

pub(super) struct LoadedVariables {
    pub info: DwarfVariableInfo,
    pub globals: Vec<GlobalVariableInfo>,
    pub types: Arc<[crate::TypeNode]>,
    /// Rust trait objects' vtables, by address, with the concrete type each
    /// is for.
    pub vtables: Vec<(ImageAddress, TypeReference)>,
    /// Integer constants the units declare at their top level, by name.
    pub constants: BTreeMap<Arc<str>, crate::IntegerValue>,
    /// The coroutine each code instance that runs one is passed, as the
    /// body of an `async fn` is passed its future.
    pub coroutine_bodies: BTreeMap<CodeInstanceId, TypeId>,
    /// Every type named as a coroutine, with what it is or why its layout
    /// cannot be read as one.
    pub coroutines: BTreeMap<TypeId, std::result::Result<crate::CoroutineInfo, Arc<str>>>,
    /// The generic type arguments of each code instance of a generic
    /// function, by their parameters' names.
    pub function_generics: BTreeMap<CodeInstanceId, crate::FunctionGenerics>,
}

/// The producer a unit names.
fn unit_producer(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
) -> std::result::Result<Option<Arc<str>>, DwarfError> {
    let mut entries = unit.entries();
    let Some(root) = entries.next_dfs()? else {
        return Ok(None);
    };
    super::string_attribute(dwarf, unit, root, gimli::DW_AT_producer)
}

/// What a function returns by the System V convention: one value of its
/// type, named for it, unless it returns nothing.
fn system_v_returns<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    units: &Units<'data>,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'data>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
    language: SourceLanguage,
    types: &mut TypeArenaBuilder<'_, 'data>,
) -> Option<returns::ReturnConvention> {
    let chain = origin_chain(units, unit_index, entry).unwrap_or_default();
    let (type_unit, type_value) = type_with_origins(unit_index, entry, &chain);
    let type_value = type_value?;
    let name = string_with_origins(dwarf, units, unit, entry, &chain, gimli::DW_AT_name)
        .ok()
        .flatten()
        .unwrap_or_else(|| Arc::from("returned"));
    let rewritten = std::iter::once(entry)
        .chain(chain.iter().map(|(_, origin)| origin))
        .any(|entry| {
            entry.attr_value(gimli::DW_AT_calling_convention)
                == Some(gimli::AttributeValue::CallingConvention(
                    gimli::DW_CC_nocall,
                ))
        });
    Some(returns::ReturnConvention::SystemV(Box::new(
        returns::SystemV {
            name,
            ty: types.variable_type(type_unit, Some(type_value)),
            language,
            rewritten,
        },
    )))
}

/// The file declaring a function or inlined call's function, if known.
fn declared_file<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    units: &Units<'data>,
    unit_index: usize,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
    files: &mut Files,
) -> Option<SourceFileId> {
    let chain = origin_chain(units, unit_index, entry).ok()?;
    declaration_with_origins(dwarf, units, &units[unit_index], entry, &chain, files)
        .ok()
        .flatten()
        .map(|declaration| declaration.file)
}

/// Each Go lexical block's code fused with its nested blocks' and inlined
/// calls', as Delve reads them: Go may place a nested block's code
/// outside its parent's ranges. Only blocks the fusion changes are listed.
fn fused_block_ranges(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
    code: &super::CodeRanges,
) -> std::result::Result<HashMap<usize, Vec<AddressRange<ImageAddress>>>, DwarfError> {
    struct Open {
        depth: isize,
        offset: usize,
        block: bool,
        /// Its own ranges, fused; a block with none takes its parent's.
        own: Vec<AddressRange<ImageAddress>>,
        ranges: Vec<AddressRange<ImageAddress>>,
    }
    fn close(
        open: Open,
        stack: &mut [Open],
        fused: &mut HashMap<usize, Vec<AddressRange<ImageAddress>>>,
    ) {
        let ranges = visibility::fused(open.ranges);
        if open.block && !open.own.is_empty() && ranges != open.own {
            fused.insert(open.offset, ranges.clone());
        }
        if let Some(parent) = stack.last_mut() {
            parent.ranges.extend(ranges);
        }
    }
    let mut fused = HashMap::new();
    let mut stack = Vec::<Open>::new();
    let mut entries = unit.entries();
    while let Some(entry) = entries.next_dfs()? {
        let depth = entry.depth();
        while stack.last().is_some_and(|open| open.depth >= depth) {
            let open = stack.pop().expect("the stack is not empty");
            close(open, &mut stack, &mut fused);
        }
        let ranges = match entry.tag() {
            gimli::DW_TAG_subprogram
            | gimli::DW_TAG_lexical_block
            | gimli::DW_TAG_inlined_subroutine => die_code_ranges(dwarf, unit, entry, code)?,
            _ => continue,
        };
        stack.push(Open {
            depth,
            offset: entry.offset().0,
            block: entry.tag() == gimli::DW_TAG_lexical_block,
            own: visibility::fused(ranges.clone()),
            ranges,
        });
    }
    while let Some(open) = stack.pop() {
        close(open, &mut stack, &mut fused);
    }
    Ok(fused)
}

/// What the image knows of its code: the instance each function DIE
/// becomes, and what decides where Go's variables are visible.
#[derive(Clone, Copy)]
pub(super) struct CodeMetadata<'a> {
    pub(super) instance_ids: &'a HashMap<DieKey, CodeInstanceId>,
    pub(super) lines: &'a [crate::model::LineEntry],
    pub(super) instances: &'a [crate::CodeInstanceInfo],
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
    code: CodeMetadata<'_>,
    files: &mut Files,
) -> std::result::Result<LoadedVariables, DwarfError> {
    let units = &catalog.units;
    let instance_ids = code.instance_ids;
    let lines = visibility::LineIndex::new(code.lines);
    let inline_calls = visibility::InlineCalls::new(code.instances);
    let mut objects = Vec::new();
    let mut functions = Vec::new();
    let mut calls = call_sites::CallSiteBuilder::default();
    let mut procedures = HashMap::new();
    let mut vtables = Vec::new();
    let mut go_function_entries = HashMap::new();
    let mut unnamed_parameters = Vec::new();
    let mut abstract_bodies = Vec::new();
    let mut function_generics = BTreeMap::new();
    let mut order = 0_u64;
    let phase = crate::span!("variables.evaluation_units");
    let evaluation_units = load_evaluation_units(units)?;
    drop(phase);
    let phase = crate::span!("variables.type_arena");
    let mut types = TypeArenaBuilder::new(
        dwarf,
        units,
        &catalog.type_signatures,
        image_id,
        target.byte_order,
    );
    drop(phase);
    let phase = crate::span!("variables.globals");
    let (mut globals, global_objects) =
        load_globals(dwarf, units, &mut objects, &mut order, files, &mut types)?;
    drop(phase);

    let phase = crate::span!("variables.main_walk");
    for (unit_index, unit) in units.iter().enumerate() {
        if is_type_unit(unit) {
            continue;
        }
        let go = evaluation_units[unit_index].language == Some(gimli::DW_LANG_Go);
        let rust = evaluation_units[unit_index].language == Some(gimli::DW_LANG_Rust);
        // Go names the register ABI its x86-64 code calls with among the
        // flags of each unit's producer, as `go1.27.1; -N -l regabi`.
        let go_registers = go
            && target.architecture == crate::Architecture::X86_64
            && unit_producer(dwarf, unit)?.is_some_and(|producer| {
                producer
                    .split_once(';')
                    .is_some_and(|(_, flags)| flags.split_whitespace().any(|flag| flag == "regabi"))
            });
        // Other languages' x86-64 code returns as the System V convention
        // says, or, for the languages that leave theirs unspecified, as it
        // for scalars.
        let language = source_language(
            evaluation_units[unit_index].language,
            types.is_zig(unit_index),
        );
        let system_v = target.architecture == crate::Architecture::X86_64
            && matches!(
                language,
                SourceLanguage::C
                    | SourceLanguage::Cpp
                    | SourceLanguage::Rust
                    | SourceLanguage::Zig
            );
        let fused_blocks = if go {
            fused_block_ranges(dwarf, unit, &catalog.code)?
        } else {
            HashMap::new()
        };
        let mut entries = unit.entries();
        let mut scopes = Vec::<Option<Scope>>::new();
        // The concrete instances of abstract functions open at this point
        // of the walk, innermost last.
        let mut concrete = Vec::<ConcreteRoutine>::new();

        while let Some(entry) = entries.next_dfs()? {
            let depth =
                usize::try_from(entry.depth()).map_err(|_| DwarfError::InvalidEntryDepth)?;
            scopes.truncate(depth);
            while concrete
                .last()
                .is_some_and(|routine| routine.depth >= depth)
            {
                let routine = concrete.pop().expect("an open routine");
                add_abstract_only_variables(
                    dwarf,
                    units,
                    &routine,
                    &mut AbstractTargets {
                        objects: &mut objects,
                        functions: &mut functions,
                        order: &mut order,
                        types: &mut types,
                        files,
                        bodies: &mut abstract_bodies,
                    },
                )?;
            }
            let parent = scopes.last().and_then(Clone::clone);
            // A concrete DIE standing for an abstract one covers it.
            if let Some(routine) = concrete.last_mut()
                && matches!(
                    entry.tag(),
                    gimli::DW_TAG_variable | gimli::DW_TAG_formal_parameter
                )
                && let Some(origin) = die_reference(
                    entry.attr_value(gimli::DW_AT_abstract_origin),
                    unit_index,
                    units,
                )?
            {
                routine.covered.insert(origin);
            }

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
                    let key = DieKey {
                        unit: unit_index,
                        offset: entry.offset().0,
                    };
                    if rust
                        && defined
                        && !ranges.is_empty()
                        && let Some(instance) = instance_ids.get(&key)
                    {
                        let generics = types.function_generics(key);
                        if !generics.is_empty() {
                            function_generics.insert(*instance, Arc::from(generics));
                        }
                    }
                    let function = functions.len();
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
                        returns: if go_registers && defined {
                            Some(returns::ReturnConvention::GoRegisters)
                        } else if system_v && defined {
                            system_v_returns(
                                dwarf, units, unit_index, unit, entry, language, &mut types,
                            )
                        } else {
                            None
                        },
                    });
                    let frame_base = copy_optional_location(
                        dwarf,
                        unit_index,
                        unit,
                        entry.attr_value(gimli::DW_AT_frame_base),
                        MetadataAbsence::NoFrameBase,
                    );
                    calls.function(dwarf, units, unit_index, entry, &ranges, frame_base.clone());
                    Some(Scope {
                        ranges,
                        lexical_depth: 0,
                        frame_base,
                        routine: true,
                        function,
                        instance: None,
                        code_instance: instance_ids
                            .get(&DieKey {
                                unit: unit_index,
                                offset: entry.offset().0,
                            })
                            .copied(),
                        go_file: if go {
                            declared_file(dwarf, units, unit_index, entry, files)
                        } else {
                            None
                        },
                        malformed: None,
                        defined,
                        rust: rust
                            .then(|| RustScope::routine(dwarf, units, unit_index, unit, entry)),
                        awaitee: None,
                    })
                }
                gimli::DW_TAG_lexical_block => parent.as_ref().map(|parent| {
                    // A Go block's code includes its nested blocks'.
                    let own = fused_blocks.get(&entry.offset().0).map_or_else(
                        || die_code_ranges(dwarf, unit, entry, &catalog.code),
                        |fused| Ok(fused.clone()),
                    );
                    let (ranges, malformed) = match own.map(Arc::<[_]>::from) {
                        Ok(ranges) if !ranges.is_empty() => (ranges, None),
                        Ok(_) => (Arc::clone(&parent.ranges), None),
                        Err(error) => (Arc::clone(&parent.ranges), Some(error.to_string().into())),
                    };
                    Scope {
                        ranges,
                        lexical_depth: parent.lexical_depth.saturating_add(1),
                        frame_base: parent.frame_base.clone(),
                        routine: parent.routine,
                        function: parent.function,
                        instance: parent.instance,
                        code_instance: parent.code_instance,
                        go_file: parent.go_file,
                        rust: parent.rust.map(|_| RustScope::Other),
                        awaitee: if parent.rust.is_some() {
                            origin_awaitee(dwarf, units, unit_index, entry).or(parent.awaitee)
                        } else {
                            None
                        },
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
                        code_instance: instance,
                        go_file: if go {
                            declared_file(dwarf, units, unit_index, entry, files)
                        } else {
                            None
                        },
                        rust: parent
                            .rust
                            .map(|_| RustScope::routine(dwarf, units, unit_index, unit, entry)),
                        awaitee: None,
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

            if matches!(
                entry.tag(),
                gimli::DW_TAG_subprogram
                    | gimli::DW_TAG_inlined_subroutine
                    | gimli::DW_TAG_lexical_block
            ) && let Some(routine) = scope.as_ref().filter(|scope| !scope.ranges.is_empty())
                && let Some(origin) = die_reference(
                    entry.attr_value(gimli::DW_AT_abstract_origin),
                    unit_index,
                    units,
                )?
            {
                concrete.push(ConcreteRoutine {
                    depth,
                    origin,
                    scope: routine.clone(),
                    covered: std::collections::HashSet::new(),
                });
            }
            match entry.tag() {
                gimli::DW_TAG_call_site | gimli::DW_TAG_GNU_call_site => {
                    if let Some(parent) = parent.as_ref().filter(|parent| parent.defined) {
                        calls.site(dwarf, units, unit_index, entry, parent.function, depth);
                    }
                }
                gimli::DW_TAG_call_site_parameter | gimli::DW_TAG_GNU_call_site_parameter => {
                    calls.parameter(dwarf, units, unit_index, entry, depth);
                }
                gimli::DW_TAG_dwarf_procedure => {
                    if let Some(offset) = debug_info_offset(unit, entry) {
                        let location = copy_optional_location(
                            dwarf,
                            unit_index,
                            unit,
                            entry.attr_value(gimli::DW_AT_location),
                            MetadataAbsence::NoLocation,
                        );
                        procedures.insert(offset, CatalogProcedure { location });
                    }
                }
                _ => {}
            }
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
                        && go
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
                        VariableKind::Result | VariableKind::Returned => "result",
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
                        dwarf, units, unit, entry, &chain, files,
                    )
                    .map(|declaration| {
                        // Go gives a variable's line alone: its file is
                        // its function's.
                        declaration.or_else(|| {
                            let line = entry
                                .attr(gimli::DW_AT_decl_line)
                                .and_then(gimli::Attribute::udata_value)
                                .and_then(crate::LineNumber::new)?;
                            Some(SourceLocation {
                                file: scope.go_file?,
                                line,
                                column: None,
                            })
                        })
                    });
                    let (ranges, scope_error) = data_object_scope_ranges(scope, entry);
                    // A Go local exists from the line after its declaration.
                    let ranges = match (go, kind, &declaration) {
                        (true, VariableKind::Local, Ok(Some(declared))) => {
                            visibility::after_declaration(
                                &ranges,
                                declared,
                                &lines,
                                inline_calls.within(scope.code_instance),
                            )
                            .into()
                        }
                        _ => ranges,
                    };
                    let (type_unit, type_value) = type_with_origins(unit_index, entry, &chain);
                    let type_info = types.variable_type(type_unit, type_value);
                    // Go names a variable it moved to the heap `&name`, and
                    // describes the pointer to it.
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
                    // Go starts the names of its own variables with
                    // characters no Go identifier can. rustc's own are an
                    // async body's temporaries and unnamed parameters, the
                    // `result` an await binds, and, in an `async fn`'s
                    // body, the fields of its future that captured its
                    // arguments, which the body moves into variables of
                    // its own.
                    let rust_unnamed = scope.rust.is_some()
                        && kind == VariableKind::Parameter
                        && name_error.is_some();
                    let declared_line = declaration
                        .as_ref()
                        .ok()
                        .and_then(|declared| declared.as_ref().map(|declared| declared.line));
                    let hidden = (go && name.starts_with(['.', '#']))
                        || (scope.rust.is_some()
                            && (rust_temporary(&name, rust_unnamed)
                                || (scope.rust == Some(RustScope::AsyncCaptures)
                                    && kind == VariableKind::Local)
                                || (&*name == "result"
                                    && declared_line.is_some()
                                    && declared_line == scope.awaitee)));
                    if scope.rust.is_some()
                        && &*name == "__awaitee"
                        && let Some(Some(enclosing)) = scopes.last_mut()
                    {
                        enclosing.awaitee = declared_line;
                    }
                    // rustc passes the body of an `async fn` its future
                    // as an unnamed parameter.
                    if kind == VariableKind::Parameter
                        && name_error.is_some()
                        && let (Some(instance), TypeResolution::Resolved(ty)) =
                            (scope.code_instance, &type_info)
                    {
                        unnamed_parameters.push((instance, *ty, objects.len()));
                    }
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
                        hidden,
                        coroutine: None,
                        value: copy_data_object_value(dwarf, unit_index, unit, entry),
                        frame_base: scope.frame_base.clone(),
                        malformed: declaration
                            .err()
                            .map(|error| error.to_string().into())
                            .or(scope_error)
                            .or_else(|| scope.malformed.clone())
                            .or(chain_error)
                            .or_else(|| name_error.filter(|_| !rust_unnamed)),
                    });
                }
            }

            scopes.push(scope);
        }
        while let Some(routine) = concrete.pop() {
            add_abstract_only_variables(
                dwarf,
                units,
                &routine,
                &mut AbstractTargets {
                    objects: &mut objects,
                    functions: &mut functions,
                    order: &mut order,
                    types: &mut types,
                    files,
                    bodies: &mut abstract_bodies,
                },
            )?;
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
    drop(phase);
    let phase = crate::span!("variables.constants_and_members");
    let objects_by_debug_offset = objects
        .iter()
        .enumerate()
        .filter_map(|(index, object)| object.debug_info_offset.map(|offset| (offset, index)))
        .collect();
    let constants = types.named_constants();
    types.populate_go_named_constants();
    types.populate_record_member_declarations(files);
    drop(phase);
    let phase = crate::span!("variables.finalize_types");
    types.finalize_type_graph();
    drop(phase);
    let _phase = crate::span!("variables.catalog");
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
    let mut go_runtime_types = HashMap::new();
    for node in finalized_types.iter() {
        if let TypeNode::Resolved(info) = node
            && let Some(offset) = info
                .identity
                .as_ref()
                .and_then(|identity| identity.go)
                .and_then(|go| go.runtime_type)
        {
            go_runtime_types.entry(offset).or_insert(info.reference.id);
        }
    }
    let coroutines = crate::debug_info::coroutines::normalize(&finalized_types);
    let mut coroutine_bodies = BTreeMap::new();
    for (instance, ty, object) in unnamed_parameters {
        if let Some(coroutine) =
            crate::debug_info::coroutines::pinned_coroutine(&finalized_types, ty)
        {
            coroutine_bodies.insert(instance, coroutine);
            // The body's future is what the parameter points to.
            let future = &mut objects[object];
            future.name = "$future".into();
            future.type_info = TypeResolution::Resolved(coroutine);
            future.escaped = Some(ty);
            future.coroutine = Some(coroutine);
        }
    }
    for (instance, ty) in abstract_bodies {
        if let Some(coroutine) =
            crate::debug_info::coroutines::pinned_coroutine(&finalized_types, ty)
        {
            coroutine_bodies.entry(instance).or_insert(coroutine);
        }
    }
    let running = coroutines
        .iter()
        .filter_map(|(ty, coroutine)| Some((*ty, coroutine.as_ref().ok()?.clone())))
        .collect();
    Ok(LoadedVariables {
        coroutines,
        coroutine_bodies,
        function_generics,
        info: DwarfVariableInfo {
            coroutines: running,
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
            passed_by_value: types.passed_by_value,
            go_dict_indices: types.go_dict_indices,
            go_runtime_types,
            objects_by_debug_offset,
            procedures,
            call_sites: calls.finish(),
            target,
            endian: match target.byte_order {
                ByteOrder::Little => RunTimeEndian::Little,
                ByteOrder::Big => RunTimeEndian::Big,
            },
            held: BTreeMap::new(),
        },
        globals,
        types: finalized_types,
        constants,
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

/// A concrete instance of an abstract function, inlined or out of line,
/// and the abstract variables its own DIEs stand for.
struct ConcreteRoutine {
    depth: usize,
    origin: DieKey,
    scope: Scope,
    covered: std::collections::HashSet<DieKey>,
}

/// Where the variables an instance leaves out are added.
struct AbstractTargets<'a, 'data, 'units> {
    objects: &'a mut Vec<CatalogDataObject>,
    functions: &'a mut Vec<CatalogFunction>,
    order: &'a mut u64,
    types: &'a mut TypeArenaBuilder<'units, 'data>,
    files: &'a mut Files,
    /// The instances whose abstract function takes an unnamed parameter,
    /// as an `async fn`'s body takes its future, and its type.
    bodies: &'a mut Vec<(CodeInstanceId, TypeId)>,
}

/// Adds the named variables and parameters of a concrete instance's
/// abstract function, or of a concrete block's abstract block, that it has
/// no DIE for: the compiler kept no trace of them there, so they exist in
/// its code without a location, as gdb shows them, rather than not at all.
/// A nested abstract block is its own concrete block's to add; one with
/// none has no code, so its variables are nowhere in scope.
fn add_abstract_only_variables<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    units: &Units<'data>,
    routine: &ConcreteRoutine,
    targets: &mut AbstractTargets<'_, 'data, '_>,
) -> std::result::Result<(), DwarfError> {
    let Some(unit) = units.get(routine.origin.unit) else {
        return Ok(());
    };
    let mut tree = unit.entries_tree(Some(gimli::UnitOffset(routine.origin.offset)))?;
    let mut children = tree.root()?.children();
    let mut awaitee = routine.scope.awaitee;
    while let Some(child) = children.next()? {
        let entry = child.entry();
        let key = DieKey {
            unit: routine.origin.unit,
            offset: entry.offset().0,
        };
        let kind = match entry.tag() {
            gimli::DW_TAG_variable => VariableKind::Local,
            gimli::DW_TAG_formal_parameter => VariableKind::Parameter,
            _ => continue,
        };
        if routine.covered.contains(&key) {
            continue;
        }
        let Some(name) = string_attribute_of(dwarf, unit, entry)? else {
            // An inlined `async fn` body may keep no DIE for the future
            // its abstract function takes, which still says what it runs.
            if kind == VariableKind::Parameter
                && routine.scope.rust.is_some()
                && let Some(instance) = routine.scope.code_instance
                && let TypeResolution::Resolved(ty) = targets
                    .types
                    .variable_type(routine.origin.unit, entry.attr_value(gimli::DW_AT_type))
            {
                targets.bodies.push((instance, ty));
            }
            continue;
        };
        let declaration = declaration_with_origins(dwarf, units, unit, entry, &[], targets.files)
            .ok()
            .flatten();
        let declared_line = declaration.as_ref().map(|declared| declared.line);
        if &*name == "__awaitee" {
            awaitee = declared_line;
        }
        let type_info = targets
            .types
            .variable_type(routine.origin.unit, entry.attr_value(gimli::DW_AT_type));
        *targets.order = targets
            .order
            .checked_add(1)
            .expect("data-object DIE order overflow");
        let hidden = routine.scope.rust.is_some()
            && (rust_temporary(&name, false)
                || (routine.scope.rust == Some(RustScope::AsyncCaptures)
                    && kind == VariableKind::Local)
                || (&*name == "result" && declared_line.is_some() && declared_line == awaitee));
        check_data_object_capacity(targets.objects.len())?;
        targets.functions[routine.scope.function]
            .objects
            .push(targets.objects.len());
        targets.objects.push(CatalogDataObject {
            debug_info_offset: None,
            kind,
            name,
            declaration,
            ranges: Arc::clone(&routine.scope.ranges),
            instance: routine.scope.instance,
            lexical_depth: routine.scope.lexical_depth,
            order: *targets.order,
            type_info,
            escaped: None,
            hidden,
            coroutine: None,
            value: Metadata::Absent(MetadataAbsence::NoLocation),
            frame_base: routine.scope.frame_base.clone(),
            malformed: routine.scope.malformed.clone(),
        });
    }
    Ok(())
}

/// The line of the await whose future an abstract block's origin declares
/// as `__awaitee`, which a concrete copy of the block may leave out.
fn origin_awaitee<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    units: &Units<'data>,
    unit_index: usize,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
) -> Option<crate::LineNumber> {
    let origin = die_reference(
        entry.attr_value(gimli::DW_AT_abstract_origin),
        unit_index,
        units,
    )
    .ok()??;
    let unit = units.get(origin.unit)?;
    let mut tree = unit
        .entries_tree(Some(gimli::UnitOffset(origin.offset)))
        .ok()?;
    let mut children = tree.root().ok()?.children();
    while let Ok(Some(child)) = children.next() {
        let entry = child.entry();
        if entry.tag() == gimli::DW_TAG_variable
            && string_attribute_of(dwarf, unit, entry)
                .ok()
                .flatten()
                .is_some_and(|name| &*name == "__awaitee")
        {
            return entry
                .attr(gimli::DW_AT_decl_line)
                .and_then(gimli::Attribute::udata_value)
                .and_then(crate::LineNumber::new);
        }
    }
    None
}

/// Whether a subprogram or inline instance is the body of a Rust `async
/// fn`, by its name or its abstract origin's.
fn async_fn_body<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    units: &Units<'data>,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'data>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
) -> bool {
    origin_chain(units, unit_index, entry)
        .ok()
        .and_then(|chain| {
            string_with_origins(dwarf, units, unit, entry, &chain, gimli::DW_AT_name)
                .ok()
                .flatten()
        })
        .is_some_and(|name| crate::debug_info::coroutines::is_async_fn_body(&name))
}

/// Whether rustc made a variable for its own use: an async body's
/// `_task_context`, an await's `__awaitee`, numbered temporaries, and
/// the parameters it leaves unnamed.
fn rust_temporary(name: &str, unnamed_parameter: bool) -> bool {
    unnamed_parameter
        || name == "_task_context"
        || name == "__awaitee"
        || name
            .strip_prefix("__")
            .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
}

fn string_attribute_of<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    unit: &gimli::Unit<Reader<'data>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
) -> std::result::Result<Option<Arc<str>>, DwarfError> {
    entry
        .attr_value(gimli::DW_AT_name)
        .map(|value| dwarf.attr_string(unit, value))
        .transpose()
        .map_err(DwarfError::from)
        .map(|value| value.map(|value| Arc::<str>::from(value.to_string_lossy().as_ref())))
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
                            && !object.hidden
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
        let resumption = self.function_at(address).and_then(|function| {
            self.resumption(
                function,
                address,
                selected,
                runtime,
                &mut frame_base,
                budget,
            )
        });
        let mut variables = Vec::new();
        for object in objects {
            if budget.consume_variable_value().is_err() {
                break;
            }
            let mut variable = self.inspect_data_object(
                object,
                Some(address),
                context,
                runtime,
                &mut frame_base,
                budget,
            )?;
            if let Some(resumption) = &resumption {
                resumption.check(object, &mut variable);
            }
            variables.push(variable);
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
        // A generic value has its type argument, laid out as its shape.
        let ty = match self.generic_type(ty, variable.instance, address, runtime, budget)? {
            generic::Generic::Resolved(argument) => argument,
            generic::Generic::Unresolved(shape, _) => shape,
            generic::Generic::Plain => ty,
        };
        match self.located_data_object(variable, address, runtime, &mut frame_base, budget) {
            Ok(storage) => {
                // A running async body's variable that the await it
                // resumed from did not keep holds what another poll left.
                let resumption = address.and_then(|address| {
                    self.resumption(
                        self.function_at(address)?,
                        address,
                        variable.instance,
                        runtime,
                        &mut frame_base,
                        budget,
                    )
                });
                let memory = match storage {
                    crate::model::ValueStorage::Memory(address) => Some(address),
                    _ => None,
                };
                if let Some(reason) =
                    resumption.and_then(|resumption| resumption.stale(variable, memory))
                {
                    return Ok(Err(VariableState::Unavailable(reason)));
                }
                Ok(Ok(Located { ty, storage }))
            }
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
                // A place a composite's piece holds is that piece's.
                let storage = match self.value_shape(ty) {
                    Ok(shape) => storage::narrow(storage, shape.byte_size()),
                    Err(_) => storage,
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

    fn text_span(
        &self,
        at: &Located,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<std::result::Result<Option<TextLocation>, VariableState>> {
        let state = self.decode_state(at.ty, &at.storage, context, runtime, budget)?;
        let VariableState::Available { value, .. } = state else {
            return Ok(Err(state));
        };
        let Ok(shape) = self.value_shape(at.ty) else {
            return Ok(Ok(None));
        };
        Ok(self
            .text_location(at.ty, &shape, &value, &at.storage, runtime, budget)
            .map_err(VariableState::Unavailable))
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

    fn returned(
        &self,
        function: ImageAddress,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Option<Vec<crate::debug_info::ReturnedValue>>> {
        self.returned_values(function, runtime, budget)
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

    fn call_site(
        &self,
        return_address: ImageAddress,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<Option<CallSite>, VariableRuntimeError> {
        self.described_call_site(return_address, runtime, budget)
    }

    fn tail_calls(
        &self,
        from: ImageAddress,
        to: ImageAddress,
    ) -> std::result::Result<crate::debug_info::TailCallChain, VariableRuntimeError> {
        self.tail_call_path(from, to)
    }

    fn call_site_value(
        &self,
        site: CallSiteId,
        parameter: EntryParameter,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<u64, VariableRuntimeError> {
        self.site_parameter_value(site, parameter, runtime, budget)
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

    use crate::debug_info::{EntryParameter, VariableRuntime, VariableRuntimeError};
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

        fn entry_value(
            &mut self,
            _parameter: EntryParameter,
            _budget: &mut InspectionBudget,
        ) -> std::result::Result<u64, VariableRuntimeError> {
            Err(VariableRuntimeError::Unavailable(
                VariableUnavailableReason::EntryValue(crate::EntryValueUnavailableReason::NoCaller),
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
        procedures: Arc::default(),
    };
    let mut budget = InspectionBudget::default();
    if let Ok(pieces) = evaluate(
        &expression,
        RunTimeEndian::Little,
        None,
        &mut FrameBase::Unsupported,
        &[],
        &mut FuzzRuntime,
        &mut budget,
    ) {
        fuzz_storage(&pieces, &mut FuzzRuntime, &mut budget);
    }
}

/// Makes fuzzed pieces the storage of a 16-byte object, and reads it whole,
/// by bits, and in part.
#[cfg(feature = "fuzzing")]
fn fuzz_storage(
    pieces: &[gimli::Piece<Reader<'_>>],
    runtime: &mut dyn crate::debug_info::VariableRuntime,
    budget: &mut InspectionBudget,
) {
    let target = TargetDescription {
        architecture: crate::Architecture::X86_64,
        pointer_width: crate::PointerWidth::Bits64,
        byte_order: ByteOrder::Little,
    };
    let Ok(storage) =
        pieces::storage_from_pieces(pieces, 16, None, RunTimeEndian::Little, target, runtime)
    else {
        return;
    };
    let _ = storage::read(&storage, 16, runtime, budget);
    let _ = storage::read_bits(&storage, 3, 61, ByteOrder::Little, runtime, budget);
    let _ = storage::offset(storage, 8).map(|storage| storage::narrow(storage, 8));
}

#[cfg(test)]
mod tests;
