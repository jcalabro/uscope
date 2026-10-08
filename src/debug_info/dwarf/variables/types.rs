//! Normalizing DWARF type DIEs into the platform-neutral type graph.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use foldhash::{HashMap, HashMapExt, HashSet, HashSetExt};

use crate::debug_info::dwarf::{DieKey, Reader, TypeSignatures, die_reference_with_signatures};
use crate::model::ArrayDimension;
use crate::{
    Accessibility, BaseClass, BaseClassVirtuality, BaseType, BaseTypeEncoding, ByteOrder,
    EnumerationOrigin, Enumerator, GoKind, IntegerValue, ModuleImageId, NamedTypeRelationship,
    RecordKind, RecordMember, RecordMemberLayout, ReferenceKind, SourceFile, SourceFileId,
    SourceLanguage, SourceLocation, TypeId, TypeInfo, TypeKind, TypeModifier, TypeReference,
    Variant, VariantDiscriminant, VariantSelection, VariantSelector, VariantStorageKind,
};

use super::codec::{complex_part, enumeration_constant};
use super::die::{
    ByteSize, DW_AT_ZIG_PARENT, UnsignedConstant, array_bound, base_type_encoding,
    byte_size_attribute, constant_member_offset, copy_name, declaration_with_origins,
    index_type_is_signed, origin_chain, strict_flag, string_with_origins, type_with_origins,
    unsigned_constant, zig_qualified_name,
};
use super::identity::{
    IdentityParts, ScopePath, ScopeSegment, go_embedded, inline_namespace_path, scope_segment,
    source_language,
};
use super::location::{Expression, copy_expression};
use super::variant::{
    VariantMetadataBudget, VariantMetadataError, copy_variant_selection,
    validate_variant_selections,
};
use super::{MAX_RECORD_CHILDREN, MAX_SYMBOLIC_NAMES, MAX_TYPE_RESOLUTION_DEPTH, MAX_TYPES};

#[derive(Clone)]
pub(super) enum TypeResolution {
    Resolved(TypeId),
    Malformed(Arc<str>),
}

#[derive(Debug, Clone)]
pub(super) enum TypeEntry {
    Building,
    Resolved(TypeInfo),
    Malformed(Arc<str>),
}

pub(super) struct TypeArenaBuilder<'a, 'data> {
    pub(super) dwarf: &'a gimli::Dwarf<Reader<'data>>,
    pub(super) units: &'a [gimli::Unit<Reader<'data>>],
    pub(super) type_signatures: &'a TypeSignatures,
    pub(super) image: ModuleImageId,
    pub(super) by_die: HashMap<DieKey, TypeId>,
    pub(super) type_definitions: HashMap<DieKey, DieKey>,
    pub(super) ambiguous_type_declarations: HashSet<DieKey>,
    pub(super) entries: Vec<TypeEntry>,
    /// Each unit's DIE offsets. A reference to any other offset points into
    /// the middle of a DIE, whose bytes could decode as convincing nonsense.
    pub(super) die_offsets: Vec<HashSet<usize>>,
    pub(super) unit_languages: Vec<Option<gimli::DwLang>>,
    pub(super) zig_units: Vec<bool>,
    pub(super) explicit_names: HashSet<TypeId>,
    pub(super) resolution_depth: usize,
    pub(super) byte_order: ByteOrder,
    pub(super) limit_type: Option<TypeId>,
    /// The shared `void` that qualifiers and typedefs without a target name.
    pub(super) void_type: Option<TypeId>,
    pub(super) dynamic_record_layouts: HashMap<DynamicAggregateLayoutKey, Expression>,
    pub(super) record_member_declarations: Vec<AggregateMemberDeclaration>,
    pub(super) symbolic_names: usize,
    /// The scopes enclosing each type DIE that has any.
    pub(super) type_scopes: HashMap<DieKey, ScopePath>,
    /// The declaration each out-of-line type definition completes.
    pub(super) definition_declarations: HashMap<DieKey, DieKey>,
    /// What each named type's identity is built from.
    pub(super) identity_parts: HashMap<TypeId, IdentityParts>,
    /// The float type each complex type's parts have, by the part's name
    /// and size.
    pub(super) complex_parts: HashMap<(Arc<str>, u64), TypeId>,
    /// Go's generic type parameters: each typedef of a shape that names
    /// its type argument's entry in the function's dictionary.
    pub(super) go_dict_indices: HashMap<TypeId, u64>,
    /// Whether each C++ class whose producer says how calls pass it is
    /// passed by value, in registers where it fits, rather than by
    /// reference to a copy.
    pub(super) passed_by_value: HashMap<TypeId, bool>,
}

#[derive(Clone, Copy)]
pub(super) struct AggregateMemberDeclaration {
    pub(super) aggregate: TypeId,
    pub(super) member: AggregateMemberPath,
    pub(super) die: DieKey,
}

#[derive(Clone, Copy)]
pub(super) enum AggregateMemberPath {
    Direct(usize),
    Discriminant,
    Variant { variant: usize, member: usize },
}

pub(super) enum NamedConstantCollection {
    Enumerators(Vec<Enumerator>),
    Malformed(Arc<str>),
    Limit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct DynamicAggregateLayoutKey {
    pub(super) aggregate: TypeId,
    pub(super) child: DynamicAggregateChild,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum DynamicAggregateChild {
    Member(usize),
    Base(usize),
    Discriminant,
    VariantMember { variant: usize, member: usize },
}

/// Whether an aggregate's child DIE describes its scope, such as a method,
/// nested type, static member, or template parameter, rather than bytes of
/// an instance. Any type may be declared in a scope: GCC nests the
/// qualified types a class's methods use in the class.
const fn is_scope_only_child(tag: gimli::DwTag) -> bool {
    is_type_die_tag(tag)
        || matches!(
            tag,
            gimli::DW_TAG_subprogram
                | gimli::DW_TAG_variable
                | gimli::DW_TAG_template_type_parameter
                | gimli::DW_TAG_template_value_parameter
                | gimli::DW_TAG_GNU_template_parameter_pack
                | gimli::DW_TAG_GNU_template_template_param
                | gimli::DW_TAG_friend
                | gimli::DW_TAG_imported_declaration
                | gimli::DW_TAG_imported_module
                | gimli::DW_TAG_access_declaration
        )
}

pub(super) fn zig_optional_payload_name(name: &str) -> Option<&str> {
    name.strip_prefix('?').filter(|payload| !payload.is_empty())
}

pub(super) fn zig_error_union_type_names(name: &str) -> Option<(&str, &str)> {
    let (error, payload) = name.split_once('!')?;
    if payload.is_empty()
        || !(error == "anyerror" || error.starts_with("error{") && error.ends_with('}'))
    {
        return None;
    }
    Some((error, payload))
}

impl<'a, 'data> TypeArenaBuilder<'a, 'data> {
    #[expect(
        clippy::too_many_lines,
        reason = "one walk of each unit collects its DIE boundaries, language, scopes, and declarations"
    )]
    pub(super) fn new(
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
        let mut definition_declarations = HashMap::new();
        let mut ambiguous_type_declarations = HashSet::new();
        let mut scoped_types = Vec::new();
        let mut inline_namespaces = HashSet::new();
        for (unit_index, unit) in units.iter().enumerate() {
            let mut offsets = HashSet::new();
            let mut language = None;
            let mut zig_producer = false;
            let mut cpp = false;
            let mut first = true;
            let mut scopes = Vec::<(isize, ScopeSegment)>::new();
            // The current scopes, shared by the types declared in them.
            let mut current = None::<Arc<[ScopeSegment]>>;
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
                    cpp = source_language(language, zig_producer) == SourceLanguage::Cpp;
                }
                let depth = entry.depth();
                while scopes.last().is_some_and(|(scope, _)| *scope >= depth) {
                    scopes.pop();
                    current = None;
                }
                let key = DieKey {
                    unit: unit_index,
                    offset: entry.offset().0,
                };
                // Paths are resolved once every unit is read, since a
                // function's name may live in another unit.
                if is_type_die_tag(entry.tag()) && !scopes.is_empty() {
                    let segments = current.get_or_insert_with(|| {
                        scopes.iter().map(|(_, segment)| segment.clone()).collect()
                    });
                    scoped_types.push((key, Arc::clone(segments)));
                }
                if let Some(segment) = scope_segment(dwarf, unit, unit_index, entry, cpp) {
                    if let ScopeSegment::Inline(name) = &segment {
                        inline_namespaces.insert(inline_namespace_path(&scopes, name));
                    }
                    scopes.push((depth, segment));
                    current = None;
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
                definition_declarations.insert(definition, declaration);
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
        let mut builder = Self {
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
            void_type: None,
            dynamic_record_layouts: HashMap::new(),
            record_member_declarations: Vec::new(),
            symbolic_names: 0,
            type_scopes: HashMap::new(),
            definition_declarations,
            identity_parts: HashMap::new(),
            complex_parts: HashMap::new(),
            passed_by_value: HashMap::new(),
            go_dict_indices: HashMap::new(),
        };
        let mut paths = HashMap::<*const ScopeSegment, ScopePath>::new();
        for (key, segments) in scoped_types {
            let path = paths
                .entry(segments.as_ptr())
                .or_insert_with(|| builder.scope_path(&segments, &inline_namespaces));
            if !path.is_empty() {
                builder.type_scopes.insert(key, path.clone());
            }
        }
        builder
    }

    pub(super) fn variable_type(
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
        match self.entries.get(id.index()) {
            Some(TypeEntry::Malformed(reason)) => TypeResolution::Malformed(Arc::clone(reason)),
            Some(TypeEntry::Building) => {
                TypeResolution::Malformed("type graph did not finish building".into())
            }
            Some(TypeEntry::Resolved(_)) => TypeResolution::Resolved(id),
            None => TypeResolution::Malformed("type ID is outside the arena".into()),
        }
    }

    /// The DIE an attribute of a DIE in unit `unit_index` refers to.
    pub(super) fn reference(
        &self,
        unit_index: usize,
        value: Option<gimli::AttributeValue<Reader<'data>>>,
    ) -> Option<DieKey> {
        die_reference_with_signatures(value, unit_index, self.units, self.type_signatures)
            .ok()
            .flatten()
    }

    /// Builds the type an attribute refers to, if any, so that the type
    /// index knows it; a malformed reference only goes unbuilt.
    pub(super) fn reach(
        &mut self,
        unit_index: usize,
        value: Option<gimli::AttributeValue<Reader<'data>>>,
    ) {
        let _ = self.type_reference(unit_index, value);
    }

    /// Builds the type an attribute of a DIE in unit `unit_index` refers to.
    fn type_reference(
        &mut self,
        unit_index: usize,
        value: Option<gimli::AttributeValue<Reader<'data>>>,
    ) -> std::result::Result<Option<TypeReference>, Arc<str>> {
        let key =
            die_reference_with_signatures(value, unit_index, self.units, self.type_signatures)
                .map_err(malformed)?;
        Ok(key.map(|key| TypeReference {
            image: self.image,
            id: self.resolve(key),
        }))
    }

    /// The direct children of the DIE at `offset` in unit `unit_index`.
    pub(super) fn children(
        &self,
        unit_index: usize,
        offset: gimli::UnitOffset,
    ) -> std::result::Result<Children<'a, 'data>, Arc<str>> {
        let units: &'a [gimli::Unit<Reader<'data>>] = self.units;
        let unit = units.get(unit_index).ok_or("DIE unit is unavailable")?;
        Ok(Children {
            cursor: unit.entries_at_offset(offset).map_err(malformed)?,
            started: false,
            done: false,
        })
    }

    pub(super) fn is_zig(&self, unit_index: usize) -> bool {
        self.zig_units.get(unit_index).copied().unwrap_or(false)
    }

    fn next_id(&self) -> TypeId {
        TypeId::new(u32::try_from(self.entries.len()).expect("bounded type count fits u32"))
    }

    /// The byte size of a built type, if it has one.
    fn byte_size_of(&self, id: TypeId) -> Option<u64> {
        match self.entries.get(id.index())? {
            TypeEntry::Resolved(info) => info.byte_size,
            TypeEntry::Building | TypeEntry::Malformed(_) => None,
        }
    }

    pub(super) fn resolve(&mut self, key: DieKey) -> TypeId {
        if let Some(id) = self.by_die.get(&key) {
            return *id;
        }
        let canonical = self.canonical_type_key(key);
        if let Ok(canonical) = canonical
            && canonical != key
        {
            let id = self.resolve(canonical);
            self.by_die.insert(key, id);
            return id;
        }
        if self.entries.len() >= MAX_TYPES {
            return self.type_limit(key);
        }
        let id = self.next_id();
        self.by_die.insert(key, id);
        self.entries.push(TypeEntry::Building);
        let entry = match canonical {
            Err(reason) => TypeEntry::Malformed(reason),
            Ok(_) if self.resolution_depth >= MAX_TYPE_RESOLUTION_DEPTH => {
                TypeEntry::Malformed("type wrapper depth exceeds its limit".into())
            }
            Ok(_) => {
                self.resolution_depth += 1;
                let entry = self.build(key, id).unwrap_or_else(TypeEntry::Malformed);
                self.resolution_depth -= 1;
                entry
            }
        };
        self.entries[id.index()] = entry;
        id
    }

    fn type_limit(&mut self, key: DieKey) -> TypeId {
        let id = if let Some(id) = self.limit_type {
            id
        } else {
            let id = self.next_id();
            self.entries.push(TypeEntry::Malformed(
                "type graph exceeds its work limit".into(),
            ));
            self.limit_type = Some(id);
            id
        };
        self.by_die.insert(key, id);
        id
    }

    /// Returns `void`. Producers omit `DW_AT_type` to mean it, so a
    /// `const void` or `typedef void T` has no target DIE to resolve.
    pub(super) fn void_type(&mut self) -> TypeReference {
        let reference = TypeReference {
            image: self.image,
            id: self.void_type.unwrap_or_else(|| self.next_id()),
        };
        if self.void_type.is_none() {
            self.entries.push(resolved(
                reference,
                "void".into(),
                None,
                TypeKind::Unspecified,
            ));
            self.void_type = Some(reference.id);
        }
        reference
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

    /// Builds the type at a DIE that [`Self::canonical_type_key`] validated
    /// as a type DIE.
    fn build(&mut self, key: DieKey, id: TypeId) -> Built {
        let units: &'a [gimli::Unit<Reader<'data>>] = self.units;
        let unit = units
            .get(key.unit)
            .ok_or("type reference is outside loaded units")?;
        let entry = unit
            .entry(gimli::UnitOffset(key.offset))
            .map_err(malformed)?;
        let reference = TypeReference {
            image: self.image,
            id,
        };
        let origins = origin_chain(units, key.unit, &entry).map_err(malformed)?;
        let explicit_name =
            string_with_origins(self.dwarf, units, unit, &entry, &origins, gimli::DW_AT_name)
                .map_err(malformed)?;
        // Self-hosted Zig names a type declared in another by its own name,
        // and says which one it is in.
        let explicit_name = match explicit_name {
            Some(name) if self.is_zig(key.unit) && entry.attr_value(DW_AT_ZIG_PARENT).is_some() => {
                Some(
                    zig_qualified_name(self.dwarf, units, key.unit, &entry, name)
                        .map_err(malformed)?,
                )
            }
            name => name,
        };
        if explicit_name.is_some() {
            self.explicit_names.insert(id);
        }
        // Validate the tag's mandatory attributes first: an unusable size
        // returns an opaque type early, which must not mask a defect.
        if let Some(defect) = self.mandatory_attribute_defect(&entry, key.unit) {
            return Err(defect);
        }
        let address_class = match resolve_address_class(&entry, reference, explicit_name.clone()) {
            Ok(address_class) => address_class,
            Err(entry) => return Ok(*entry),
        };
        let explicit_size = match resolve_explicit_size(&entry, reference, explicit_name.clone()) {
            Ok(size) => size,
            Err(entry) => return Ok(*entry),
        };
        let pointer_size = explicit_size
            .or_else(|| (address_class == 0).then_some(u64::from(unit.encoding().address_size)));
        let named = explicit_name.is_some();
        let slice_layout = if entry.tag() == gimli::DW_TAG_structure_type {
            self.slice_layout(&entry, key, explicit_name.as_deref())
        } else {
            None
        };
        let built = match slice_layout {
            Some(layout) => self.build_slice_type(
                &entry,
                key.unit,
                reference,
                explicit_name,
                explicit_size,
                layout,
            ),
            None => self.build_kind(
                &entry,
                key.unit,
                reference,
                explicit_name,
                explicit_size,
                pointer_size,
                address_class,
            ),
        }?;
        if named && matches!(built, TypeEntry::Resolved(_)) {
            self.record_identity_parts(&entry, key, id);
        }
        Ok(built)
    }

    /// Builds a type by its tag.
    #[expect(
        clippy::too_many_arguments,
        reason = "the tag's builders take the attributes validated for every type"
    )]
    fn build_kind(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
        pointer_size: Option<u64>,
        address_class: u64,
    ) -> Built {
        match entry.tag() {
            gimli::DW_TAG_base_type => {
                Self::build_base_type(entry, reference, explicit_name, explicit_size)
            }
            gimli::DW_TAG_enumeration_type => self.build_enumeration_type(
                entry,
                unit_index,
                reference,
                explicit_name,
                explicit_size,
            ),
            gimli::DW_TAG_pointer_type => self.build_pointer_type(
                entry,
                unit_index,
                reference,
                explicit_name,
                pointer_size,
                address_class,
            ),
            gimli::DW_TAG_reference_type | gimli::DW_TAG_rvalue_reference_type => self
                .build_reference_type(
                    entry,
                    unit_index,
                    reference,
                    explicit_name,
                    pointer_size,
                    address_class,
                ),
            gimli::DW_TAG_array_type => {
                self.build_array_type(entry, unit_index, reference, explicit_name, explicit_size)
            }
            gimli::DW_TAG_structure_type | gimli::DW_TAG_class_type => {
                self.build_record_type(entry, unit_index, reference, explicit_name, explicit_size)
            }
            gimli::DW_TAG_union_type => {
                self.build_union_type(entry, unit_index, reference, explicit_name, explicit_size)
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
                self.build_wrapper_type(entry, unit_index, reference, explicit_name, explicit_size)
            }
            gimli::DW_TAG_unspecified_type => Ok(resolved(
                reference,
                explicit_name.unwrap_or_else(|| Arc::from("void")),
                explicit_size,
                TypeKind::Unspecified,
            )),
            // A Go func is a pointer to its closure context.
            gimli::DW_TAG_subroutine_type
                if self.language(unit_index) == SourceLanguage::Go
                    && Self::go_kind(entry) == Some(GoKind::Func)
                    && explicit_size == Some(8) =>
            {
                Ok(resolved(
                    reference,
                    explicit_name.unwrap_or_else(|| Arc::from("func")),
                    explicit_size,
                    TypeKind::Function,
                ))
            }
            gimli::DW_TAG_subroutine_type => {
                self.build_signature_type(entry, unit_index, reference, explicit_name)
            }
            // A type this backend does not model is opaque, not defective.
            tag => Ok(opaque(
                reference,
                explicit_name.unwrap_or_else(|| Arc::from(format!("{tag:?}"))),
                explicit_size,
                format!("type tag {tag:?} is unsupported"),
            )),
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
        entry
            .attr(gimli::DW_AT_data_member_location)
            .and_then(constant_member_offset)
            .map_or(RecordMemberLayout::Runtime, RecordMemberLayout::ByteOffset)
    }

    /// A defect in the attributes a type's tag requires.
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
                strict_flag(entry, gimli::DW_AT_declaration)
                    .err()
                    .or_else(|| {
                        self.target_defect(
                            entry,
                            unit_index,
                            "named type",
                            TargetRequirement::Optional,
                        )
                    })
            }
            gimli::DW_TAG_array_type
            | gimli::DW_TAG_coarray_type
            | gimli::DW_TAG_set_type
            | gimli::DW_TAG_file_type
            | gimli::DW_TAG_dynamic_type
            | gimli::DW_TAG_ptr_to_member_type
            | gimli::DW_TAG_packed_type
            | gimli::DW_TAG_shared_type => {
                self.target_defect(entry, unit_index, "type", TargetRequirement::Required)
            }
            // A pointer's target is optional (`void *`), but must be a type.
            _ => self.target_defect(entry, unit_index, "type", TargetRequirement::Optional),
        }
    }

    /// A defect in a `DW_AT_type` target: absent though required, or not a
    /// type DIE.
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
    ) -> Built {
        let name = explicit_name.unwrap_or_else(|| Arc::from("<unnamed base type>"));
        let byte_size = explicit_size.ok_or("base type has no byte size")?;
        if byte_size == 0 {
            // Zig's storage-less `void` payload and Rust's unit type `()` are
            // zero-byte base types with no bits to decode, whatever their
            // encoding: `void` is unspecified, and the others hold nothing,
            // as an empty structure does.
            let kind = if name.as_ref() == "void" {
                TypeKind::Unspecified
            } else {
                TypeKind::Record {
                    kind: RecordKind::Struct,
                    members: Arc::from([]),
                    bases: Arc::from([]),
                    incomplete: false,
                }
            };
            return Ok(resolved(reference, name, Some(0), kind));
        }
        let raw_encoding = gimli::DwAte(base_type_encoding(entry)?);
        let encoding = match raw_encoding {
            gimli::DW_ATE_float => Some(BaseTypeEncoding::Floating),
            gimli::DW_ATE_complex_float if byte_size % 2 == 0 => {
                Some(BaseTypeEncoding::ComplexFloating)
            }
            // `char8_t`, `char16_t`, `char32_t`, and Rust's `char` are code
            // units of UTF-8, UTF-16, and UTF-32.
            gimli::DW_ATE_UTF if matches!(byte_size, 1 | 2 | 4) => {
                Some(BaseTypeEncoding::UnsignedCharacter)
            }
            // C++'s `wchar_t` is a type of its own, which compilers encode
            // as an integer; on Linux it holds UTF-32.
            gimli::DW_ATE_signed if name.as_ref() == "wchar_t" => {
                Some(BaseTypeEncoding::SignedCharacter)
            }
            gimli::DW_ATE_unsigned if name.as_ref() == "wchar_t" => {
                Some(BaseTypeEncoding::UnsignedCharacter)
            }
            _ => integer_encoding(raw_encoding),
        };
        let Some(encoding) = encoding else {
            return Ok(opaque(
                reference,
                name,
                Some(byte_size),
                format!("base type encoding {raw_encoding:?} is unsupported"),
            ));
        };
        let bit_size = match entry.attr(gimli::DW_AT_bit_size).map(unsigned_constant) {
            None => None,
            Some(UnsignedConstant::Value(0)) => {
                return Err("base type has a zero bit size".into());
            }
            Some(UnsignedConstant::Value(bit_size)) if bit_size <= byte_size.saturating_mul(8) => {
                Some(bit_size)
            }
            Some(UnsignedConstant::Value(_) | UnsignedConstant::Oversized) => {
                return Err("base type bit size exceeds its byte storage".into());
            }
            Some(UnsignedConstant::NonConstant) => {
                return Err("DW_AT_bit_size is not an unsigned integer constant".into());
            }
        };
        let base = BaseType {
            name: Arc::clone(&name),
            base_name: Arc::clone(&name),
            encoding,
            byte_size,
            bit_size,
        };
        Ok(resolved(
            reference,
            name,
            Some(byte_size),
            TypeKind::Base(base),
        ))
    }

    pub(super) fn resolved_integer_base(
        &self,
        id: TypeId,
    ) -> std::result::Result<BaseType, Arc<str>> {
        let mut current = id;
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(current) {
                return Err("enumeration underlying type contains a wrapper cycle".into());
            }
            let entry = self
                .entries
                .get(current.index())
                .ok_or_else(|| Arc::from("enumeration underlying type is outside the arena"))?;
            let info = match entry {
                TypeEntry::Resolved(info) => info,
                TypeEntry::Malformed(reason) => return Err(Arc::clone(reason)),
                TypeEntry::Building => {
                    return Err("enumeration underlying type did not finish building".into());
                }
            };
            match &info.kind {
                TypeKind::Base(base) if !is_floating(base.encoding) => {
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
    ) -> Built {
        let name = explicit_name.unwrap_or_else(|| {
            Arc::from(format!("<anonymous enumeration@0x{:x}>", entry.offset().0))
        });
        let underlying = self.target(entry, unit_index)?;
        let mut representation = if let Some(underlying) = underlying {
            self.resolved_integer_base(underlying.id)?
        } else {
            let byte_size = explicit_size
                .ok_or("enumeration has neither an underlying type nor a byte size")?;
            if byte_size == 0 {
                return Err("enumeration has a zero byte size".into());
            }
            let Ok(raw_encoding) = base_type_encoding(entry) else {
                return Err("enumeration without an underlying type has no encoding".into());
            };
            BaseType {
                name: Arc::clone(&name),
                base_name: Arc::clone(&name),
                encoding: integer_encoding(gimli::DwAte(raw_encoding))
                    .ok_or("enumeration encoding is not an integral encoding")?,
                byte_size,
                bit_size: None,
            }
        };
        let byte_size = explicit_size.unwrap_or(representation.byte_size);
        if byte_size == 0 {
            return Err("enumeration has a zero byte size".into());
        }
        if byte_size != representation.byte_size {
            return Err("enumeration byte size differs from its underlying type".into());
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
                return Err("enumeration encoding differs from its underlying type".into());
            }
        }
        if let Some(attribute) = entry.attr(gimli::DW_AT_bit_size) {
            representation.bit_size = match unsigned_constant(attribute) {
                UnsignedConstant::Value(0) => {
                    return Err("enumeration has a zero bit size".into());
                }
                UnsignedConstant::Value(value) if value <= byte_size.saturating_mul(8) => {
                    Some(value)
                }
                UnsignedConstant::Value(_)
                | UnsignedConstant::Oversized
                | UnsignedConstant::NonConstant => {
                    return Err("enumeration bit size is not valid for its byte storage".into());
                }
            };
        }
        representation.name = Arc::clone(&name);

        let unit = &self.units[unit_index];
        let mut enumerators = Vec::new();
        for child in self.children(unit_index, entry.offset())? {
            let child = child?;
            // rustc declares an enumeration's methods inside it, and its
            // generic arguments, none of which change its values.
            if matches!(
                child.tag(),
                gimli::DW_TAG_subprogram
                    | gimli::DW_TAG_template_type_parameter
                    | gimli::DW_TAG_template_value_parameter
            ) {
                continue;
            }
            if child.tag() != gimli::DW_TAG_enumerator {
                return Err(format!(
                    "enumeration contains unsupported direct child {:?}",
                    child.tag()
                )
                .into());
            }
            if enumerators.len() >= MAX_RECORD_CHILDREN || self.symbolic_names >= MAX_SYMBOLIC_NAMES
            {
                return Ok(opaque(
                    reference,
                    name,
                    Some(byte_size),
                    "enumerator metadata exceeds its resource limit",
                ));
            }
            self.symbolic_names += 1;
            let enumerator_name = copy_name(self.dwarf, unit, &child)
                .map_err(malformed)?
                .ok_or("enumerator has no name")?;
            let value = child
                .attr_value(gimli::DW_AT_const_value)
                .ok_or("enumerator has no constant value")?;
            enumerators.push(Enumerator {
                name: enumerator_name,
                value: enumeration_constant(value, &representation, self.byte_order)?,
            });
        }
        let scoped = strict_flag(entry, gimli::DW_AT_enum_class)?;
        Ok(resolved(
            reference,
            name,
            Some(byte_size),
            TypeKind::Enumeration {
                representation,
                underlying,
                enumerators: enumerators.into(),
                origin: EnumerationOrigin::Language,
                scoped,
            },
        ))
    }

    /// A function's type: what it returns and what its parameters are.
    fn build_signature_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
    ) -> Built {
        let returns = self.target(entry, unit_index)?;
        let mut parameters = Vec::new();
        let mut variadic = false;
        for child in self.children(unit_index, entry.offset())? {
            let child = child?;
            match child.tag() {
                gimli::DW_TAG_formal_parameter => {
                    if parameters.len() >= MAX_RECORD_CHILDREN {
                        return Ok(opaque(
                            reference,
                            explicit_name.unwrap_or_else(|| Arc::from("<function>")),
                            None,
                            "parameter metadata exceeds its resource limit",
                        ));
                    }
                    parameters.push(
                        self.target(&child, unit_index)?
                            .ok_or("a function type's parameter has no type")?,
                    );
                }
                gimli::DW_TAG_unspecified_parameters => variadic = true,
                // Producers may describe more, such as template parameters,
                // which a signature does not need.
                _ => {}
            }
        }
        let prototyped = strict_flag(entry, gimli::DW_AT_prototyped)?;
        let kind = TypeKind::Signature {
            returns,
            parameters: parameters.into(),
            variadic,
            prototyped,
        };
        // Named from its parts once every type is built.
        let name = explicit_name.unwrap_or_else(|| Arc::from("<function>"));
        Ok(resolved(reference, name, None, kind))
    }

    pub(super) fn target(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> std::result::Result<Option<TypeReference>, Arc<str>> {
        self.type_reference(unit_index, entry.attr_value(gimli::DW_AT_type))
    }

    /// Every integer constant a unit declares at its top level, such as a
    /// Go package's `const`s, by name. Constants of other types are left
    /// out; values come from each constant's own type, read before typed Go
    /// constants turn their types into enumerations.
    pub(super) fn named_constants(&mut self) -> BTreeMap<Arc<str>, IntegerValue> {
        let mut constants = BTreeMap::new();
        for (unit_index, unit) in self.units.iter().enumerate() {
            let mut entries = unit.entries();
            while let Ok(Some(entry)) = entries.next_dfs() {
                if entry.depth() != 1 || entry.tag() != gimli::DW_TAG_constant {
                    continue;
                }
                let (Ok(Some(name)), Some(value)) = (
                    copy_name(self.dwarf, unit, entry),
                    entry.attr_value(gimli::DW_AT_const_value),
                ) else {
                    continue;
                };
                let Ok(Some(target)) = self.target(entry, unit_index) else {
                    continue;
                };
                let Some(TypeEntry::Resolved(TypeInfo {
                    kind: TypeKind::Base(base),
                    ..
                })) = self.entries.get(target.id.index())
                else {
                    continue;
                };
                if matches!(base.encoding, BaseTypeEncoding::Floating) {
                    continue;
                }
                if let Ok(value) = enumeration_constant(value, base, self.byte_order) {
                    constants.insert(name, value);
                }
            }
        }
        constants
    }

    pub(super) fn populate_go_named_constants(&mut self) {
        let mut constants = BTreeMap::<TypeId, NamedConstantCollection>::new();
        for (unit_index, unit) in self.units.iter().enumerate() {
            if self.language(unit_index) != SourceLanguage::Go {
                continue;
            }
            let mut entries = unit.entries();
            while let Ok(Some(entry)) = entries.next_dfs() {
                if entry.tag() != gimli::DW_TAG_constant {
                    continue;
                }
                let Ok(Some(target)) = self.target(entry, unit_index) else {
                    continue;
                };
                let target = target.id;
                let Some(TypeEntry::Resolved(info)) = self.entries.get(target.index()) else {
                    continue;
                };
                let TypeKind::Base(base) = &info.kind else {
                    continue;
                };
                if !info.name.contains('.') || is_floating(base.encoding) {
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
            let index = target.index();
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

    pub(super) fn populate_record_member_declarations(
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
                    self.entries[record.index()] = TypeEntry::Malformed(reason);
                    continue;
                }
            };
            let Some(TypeEntry::Resolved(info)) = self.entries.get_mut(record.index()) else {
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
                        VariantDiscriminant::TagType(_) | VariantDiscriminant::Absent => {}
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
            .get(target.id.index())
            .and_then(|entry| match entry {
                TypeEntry::Resolved(info) => Some(Arc::clone(&info.name)),
                TypeEntry::Building | TypeEntry::Malformed(_) => None,
            })
            .unwrap_or_else(|| Arc::from("<recursive type>"))
    }

    pub(super) fn finalize_type_graph(&mut self) {
        for (index, entry) in self.entries.iter_mut().enumerate() {
            if matches!(entry, TypeEntry::Building) {
                *entry = TypeEntry::Malformed(
                    format!("type graph node {index} did not finish building").into(),
                );
            }
        }
        self.add_complex_parts();

        propagate_wrapper_sizes(&mut self.entries);

        self.reject_inline_storage_cycles();

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
        self.assign_identities();
    }

    /// The type a built pointer type points to.
    pub(super) fn pointee(&self, pointer: TypeId) -> Option<TypeId> {
        match self.entries.get(pointer.index()) {
            Some(TypeEntry::Resolved(TypeInfo {
                kind:
                    TypeKind::Pointer {
                        target: Some(target),
                        address_class: 0,
                    },
                ..
            })) => Some(target.id),
            _ => None,
        }
    }

    /// Gives each complex type's real and imaginary parts a float type:
    /// the program's own float of that name and size, or one made for it.
    fn add_complex_parts(&mut self) {
        let mut parts = Vec::new();
        for (index, entry) in self.entries.iter().enumerate() {
            let TypeEntry::Resolved(TypeInfo {
                kind: TypeKind::Base(base),
                ..
            }) = entry
            else {
                continue;
            };
            match base.encoding {
                BaseTypeEncoding::Floating => {
                    let id =
                        TypeId::new(u32::try_from(index).expect("bounded type count fits u32"));
                    self.complex_parts
                        .entry((Arc::clone(&base.base_name), base.byte_size))
                        .or_insert(id);
                }
                BaseTypeEncoding::ComplexFloating => parts.push(complex_part(base)),
                _ => {}
            }
        }
        for part in parts {
            let key = (Arc::clone(&part.base_name), part.byte_size);
            if self.complex_parts.contains_key(&key) || self.entries.len() >= MAX_TYPES {
                continue;
            }
            let reference = TypeReference {
                image: self.image,
                id: self.next_id(),
            };
            self.entries.push(resolved(
                reference,
                Arc::clone(&part.name),
                Some(part.byte_size),
                TypeKind::Base(part),
            ));
            self.explicit_names.insert(reference.id);
            self.complex_parts.insert(key, reference.id);
        }
    }

    fn reject_inline_storage_cycles(&mut self) {
        let description: Arc<str> = "type graph contains an inline-storage cycle".into();
        for node in inline_storage_cycle_nodes(&self.entries) {
            self.entries[node] = TypeEntry::Malformed(Arc::clone(&description));
        }
    }

    /// Renders a type's source-style name from its targets' names. Cycles
    /// and chains deeper than the resolution limit, which only malformed
    /// metadata builds, render as `<type #N>` instead of recursing without
    /// bound on the controller thread's stack.
    fn render_type_name(&self, id: TypeId, visiting: &mut HashSet<TypeId>) -> Arc<str> {
        if visiting.len() >= MAX_TYPE_RESOLUTION_DEPTH || !visiting.insert(id) {
            return Arc::from(format!("<type #{}>", id.get()));
        }
        let Some(TypeEntry::Resolved(info)) = self.entries.get(id.index()) else {
            visiting.remove(&id);
            return Arc::from(format!("<type #{}>", id.get()));
        };
        if self.explicit_names.contains(&id) {
            visiting.remove(&id);
            return Arc::clone(&info.name);
        }

        let rendered = match &info.kind {
            TypeKind::Pointer { .. }
            | TypeKind::Reference { .. }
            | TypeKind::Array { .. }
            | TypeKind::Signature { .. } => {
                // `declare` guards against cycles itself.
                visiting.remove(&id);
                let declared = self.declare(id, String::new(), visiting);
                visiting.insert(id);
                Arc::from(declared)
            }
            // A qualified pointer qualifies its declarator, as in
            // `int (* const)(int)`.
            TypeKind::Modified { modifier, target }
                if modifier_keyword(*modifier).is_some()
                    && self.modified_target_is_indirection(target.id) =>
            {
                visiting.remove(&id);
                let declared = self.declare(id, String::new(), visiting);
                visiting.insert(id);
                Arc::from(declared)
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
            let Some(TypeEntry::Resolved(info)) = self.entries.get(current.index()) else {
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

    /// Renders a type as C declares it around `inner`, the declarator
    /// built so far: `int (*)(int)` is a pointer to `int (int)`, whose
    /// parameter list binds tighter than its `*`.
    fn declare(&self, id: TypeId, inner: String, visiting: &mut HashSet<TypeId>) -> String {
        let join = |base: &str, inner: String| {
            if inner.is_empty() {
                base.to_owned()
            } else if inner.starts_with('[') {
                format!("{base}{inner}")
            } else {
                format!("{base} {inner}")
            }
        };
        let info = match self.entries.get(id.index()) {
            Some(TypeEntry::Resolved(
                info @ TypeInfo {
                    kind:
                        TypeKind::Pointer { .. }
                        | TypeKind::Reference { .. }
                        | TypeKind::Array { .. }
                        | TypeKind::Signature { .. },
                    ..
                },
            )) if !self.explicit_names.contains(&id) => info,
            Some(TypeEntry::Resolved(
                info @ TypeInfo {
                    kind: TypeKind::Modified { modifier, target },
                    ..
                },
            )) if !self.explicit_names.contains(&id)
                && modifier_keyword(*modifier).is_some()
                && self.modified_target_is_indirection(target.id) =>
            {
                info
            }
            _ => return join(&self.render_type_name(id, visiting), inner),
        };
        if visiting.len() >= MAX_TYPE_RESOLUTION_DEPTH || !visiting.insert(id) {
            return join(&format!("<type #{}>", id.get()), inner);
        }
        let declared = match &info.kind {
            TypeKind::Pointer { target: None, .. } => join("void", format!("*{inner}")),
            TypeKind::Pointer {
                target: Some(target),
                ..
            } => self.declare_indirection(target.id, "*", &inner, visiting),
            TypeKind::Reference { kind, target, .. } => {
                let symbol = if *kind == ReferenceKind::Lvalue {
                    "&"
                } else {
                    "&&"
                };
                self.declare_indirection(target.id, symbol, &inner, visiting)
            }
            TypeKind::Array {
                element,
                dimensions,
            } => {
                use std::fmt::Write;
                let mut inner = inner;
                for dimension in dimensions.iter() {
                    let _ = write!(inner, "[{}]", dimension.count);
                }
                self.declare(element.id, inner, visiting)
            }
            TypeKind::Signature {
                returns,
                parameters,
                variadic,
                prototyped,
            } => {
                let mut list = parameters
                    .iter()
                    .map(|parameter| self.render_type_name(parameter.id, visiting).to_string())
                    .collect::<Vec<_>>();
                // An unprototyped C function's `()` says nothing of its
                // parameters, though producers describe them as variadic.
                let unprototyped = list.is_empty() && !*prototyped;
                if *variadic && !unprototyped {
                    list.push("...".to_owned());
                } else if list.is_empty() && *prototyped {
                    list.push("void".to_owned());
                }
                let inner = format!("{inner}({})", list.join(", "));
                match returns {
                    Some(returns) => self.declare(returns.id, inner, visiting),
                    None => join("void", inner),
                }
            }
            TypeKind::Modified { modifier, target } => {
                let keyword = modifier_keyword(*modifier).expect("a qualifier's keyword");
                let inner = if inner.starts_with(['*', '&']) {
                    format!(" {keyword} {inner}")
                } else {
                    format!(" {keyword}{inner}")
                };
                self.declare(target.id, inner, visiting)
            }
            _ => unreachable!("only declarators are declared"),
        };
        visiting.remove(&id);
        declared
    }

    /// Declares a pointer or reference to `target`, parenthesized where
    /// the target's own declarator binds tighter, as an array's or a
    /// function's does.
    fn declare_indirection(
        &self,
        target: TypeId,
        symbol: &str,
        inner: &str,
        visiting: &mut HashSet<TypeId>,
    ) -> String {
        let binds_tighter = !self.explicit_names.contains(&target)
            && matches!(
                self.entries.get(target.index()),
                Some(TypeEntry::Resolved(TypeInfo {
                    kind: TypeKind::Array { .. } | TypeKind::Signature { .. },
                    ..
                }))
            );
        let inner = if binds_tighter {
            format!("({symbol}{inner})")
        } else {
            format!("{symbol}{inner}")
        };
        self.declare(target, inner, visiting)
    }

    fn build_pointer_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        byte_size: Option<u64>,
        address_class: u64,
    ) -> Built {
        let target = self.target(entry, unit_index)?;
        let name = explicit_name.unwrap_or_else(|| {
            target.map_or_else(
                || Arc::from("void *"),
                |target| Arc::from(format!("{} *", self.target_name(target))),
            )
        });
        Ok(resolved(
            reference,
            name,
            byte_size,
            TypeKind::Pointer {
                target,
                address_class,
            },
        ))
    }

    fn build_reference_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        byte_size: Option<u64>,
        address_class: u64,
    ) -> Built {
        let target = self
            .target(entry, unit_index)?
            .ok_or("reference type has no target")?;
        let (kind, suffix) = if entry.tag() == gimli::DW_TAG_reference_type {
            (ReferenceKind::Lvalue, "&")
        } else {
            (ReferenceKind::Rvalue, "&&")
        };
        let name = explicit_name
            .unwrap_or_else(|| Arc::from(format!("{} {suffix}", self.target_name(target))));
        Ok(resolved(
            reference,
            name,
            byte_size,
            TypeKind::Reference {
                kind,
                target,
                address_class,
            },
        ))
    }

    fn build_wrapper_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
    ) -> Built {
        let target = self.target(entry, unit_index)?;
        // An absent target means `void`, except on a typedef declaration,
        // which is incomplete.
        let declaration = strict_flag(entry, gimli::DW_AT_declaration).unwrap_or(false);
        let target = match target {
            None if !declaration => Some(self.void_type()),
            target => target,
        };
        let byte_size =
            explicit_size.or_else(|| target.and_then(|target| self.byte_size_of(target.id)));
        if matches!(
            entry.tag(),
            gimli::DW_TAG_typedef | gimli::DW_TAG_template_alias
        ) {
            let name = explicit_name.unwrap_or_else(|| {
                target.map_or_else(
                    || Arc::from("<incomplete named type>"),
                    |target| self.target_name(target),
                )
            });
            let relationship = named_type_relationship(entry.tag(), self.language(unit_index));
            if let Some(index) = entry
                .attr_value(DW_AT_GO_DICT_INDEX)
                .and_then(|value| value.udata_value())
            {
                self.go_dict_indices.insert(reference.id, index);
            }
            return Ok(resolved(
                reference,
                name,
                byte_size,
                TypeKind::Named {
                    target,
                    relationship,
                },
            ));
        }
        // Only a declaration keeps an absent target; a qualifier cannot be one.
        let target = target.ok_or("type modifier has no target")?;
        let modifier =
            type_modifier(entry.tag()).expect("modifier wrapper tags were matched by caller");
        let name = explicit_name
            .unwrap_or_else(|| Arc::from(format!("{modifier:?} {}", self.target_name(target))));
        Ok(resolved(
            reference,
            name,
            byte_size,
            TypeKind::Modified { modifier, target },
        ))
    }

    fn has_direct_variant_part(
        &self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> std::result::Result<bool, Arc<str>> {
        let mut found = false;
        for child in self.children(unit_index, entry.offset())? {
            if child?.tag() == gimli::DW_TAG_variant_part {
                if found {
                    return Err("aggregate contains multiple direct variant parts".into());
                }
                found = true;
            }
        }
        Ok(found)
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
        if incomplete || !self.is_zig(unit_index) || members.len() != 2 {
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
        let payload_info = self.entries.get(payload.type_ref.id.index())?;
        let tag_info = self.entries.get(tag.type_ref.id.index())?;
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
                .get(payload_member.type_ref.id.index())
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
        if incomplete || !self.is_zig(unit_index) || members.len() != 2 {
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
                        | BaseTypeEncoding::ComplexFloating
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

    /// Builds one member of the aggregate `aggregate`, recording its
    /// declaration and any location computed at run time.
    #[expect(
        clippy::too_many_arguments,
        reason = "a member keeps its owner, location identity, access, and declaration provenance"
    )]
    fn build_member(
        &mut self,
        child: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        aggregate: TypeId,
        dynamic_child: DynamicAggregateChild,
        declaration: AggregateMemberPath,
        absent_byte_offset: Option<u64>,
        record_kind: RecordKind,
        what: &str,
    ) -> std::result::Result<RecordMember, Arc<str>> {
        let units: &'a [gimli::Unit<Reader<'data>>] = self.units;
        let chain = origin_chain(units, unit_index, child).map_err(malformed)?;
        let (owner, value) = type_with_origins(unit_index, child, &chain);
        let target = self
            .type_reference(owner, value)?
            .ok_or_else(|| Arc::from(format!("{what} has no type")))?;
        let name = string_with_origins(
            self.dwarf,
            units,
            &units[unit_index],
            child,
            &chain,
            gimli::DW_AT_name,
        )
        .map_err(malformed)?;
        let layout = self.record_member_layout(child, target, absent_byte_offset)?;
        if layout == RecordMemberLayout::Runtime {
            self.record_dynamic_layout(child, unit_index, aggregate, dynamic_child)?;
        }
        let member = RecordMember {
            name,
            type_ref: target,
            layout,
            accessibility: Self::record_accessibility(child, record_kind)?,
            artificial: strict_flag(child, gimli::DW_AT_artificial)?,
            embedded: go_embedded(child),
            declaration: None,
        };
        self.record_member_declarations
            .push(AggregateMemberDeclaration {
                aggregate,
                member: declaration,
                die: DieKey {
                    unit: unit_index,
                    offset: child.offset().0,
                },
            });
        Ok(member)
    }

    /// Builds the `index`th base class of the aggregate `aggregate`.
    fn build_base(
        &mut self,
        child: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        aggregate: TypeId,
        index: usize,
        record_kind: RecordKind,
        what: &str,
    ) -> std::result::Result<BaseClass, Arc<str>> {
        let type_ref = self
            .target(child, unit_index)?
            .ok_or_else(|| Arc::from(format!("{what} has no type")))?;
        let layout = Self::record_byte_layout(child);
        if layout == RecordMemberLayout::Runtime {
            self.record_dynamic_layout(
                child,
                unit_index,
                aggregate,
                DynamicAggregateChild::Base(index),
            )?;
        }
        let virtuality = match child.attr_value(gimli::DW_AT_virtuality) {
            None | Some(gimli::AttributeValue::Virtuality(gimli::DW_VIRTUALITY_none)) => {
                BaseClassVirtuality::None
            }
            Some(gimli::AttributeValue::Virtuality(value))
                if value == gimli::DW_VIRTUALITY_virtual
                    || value == gimli::DW_VIRTUALITY_pure_virtual =>
            {
                BaseClassVirtuality::Virtual
            }
            _ => return Err("base-class virtuality has an invalid encoding".into()),
        };
        Ok(BaseClass {
            type_ref,
            layout,
            accessibility: Self::record_accessibility(child, record_kind)?,
            virtuality,
        })
    }

    /// Records the location expression of a member whose offset is computed
    /// at run time.
    fn record_dynamic_layout(
        &mut self,
        child: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        aggregate: TypeId,
        dynamic_child: DynamicAggregateChild,
    ) -> std::result::Result<(), Arc<str>> {
        let Some(expression) = child
            .attr_value(gimli::DW_AT_data_member_location)
            .and_then(|value| value.exprloc_value())
        else {
            return Ok(());
        };
        let unit = &self.units[unit_index];
        let expression = copy_expression(self.dwarf, unit_index, unit, expression, unit.encoding())
            .map_err(malformed)?;
        self.dynamic_record_layouts.insert(
            DynamicAggregateLayoutKey {
                aggregate,
                child: dynamic_child,
            },
            expression,
        );
        Ok(())
    }

    fn build_variant_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
        storage: VariantStorageKind,
    ) -> Built {
        let incomplete = strict_flag(entry, gimli::DW_AT_declaration)?;
        if !incomplete && explicit_size.is_none() {
            return Err("complete variant aggregate has no byte size".into());
        }
        let name = explicit_name
            .unwrap_or_else(|| Arc::from(format!("<anonymous variant@0x{:x}>", entry.offset().0)));
        let limit = || {
            opaque(
                reference,
                Arc::clone(&name),
                explicit_size,
                "variant metadata exceeds its resource limit",
            )
        };
        let mut budget = VariantMetadataBudget::default();
        let record_kind = if storage == VariantStorageKind::Class {
            RecordKind::Class
        } else {
            RecordKind::Struct
        };
        let mut common_members = Vec::new();
        let mut bases = Vec::new();
        let mut part = None;
        for child in self.children(unit_index, entry.offset())? {
            let child = child?;
            match child.tag() {
                gimli::DW_TAG_member => {
                    if budget.consume().is_err() {
                        return Ok(limit());
                    }
                    let index = common_members.len();
                    common_members.push(self.build_member(
                        &child,
                        unit_index,
                        reference.id,
                        DynamicAggregateChild::Member(index),
                        AggregateMemberPath::Direct(index),
                        (storage == VariantStorageKind::Union).then_some(0),
                        record_kind,
                        "variant component",
                    )?);
                }
                gimli::DW_TAG_inheritance if storage != VariantStorageKind::Union => {
                    if budget.consume().is_err() {
                        return Ok(limit());
                    }
                    bases.push(self.build_base(
                        &child,
                        unit_index,
                        reference.id,
                        bases.len(),
                        record_kind,
                        "variant base class",
                    )?);
                }
                gimli::DW_TAG_variant_part => {
                    if part.is_some() {
                        return Err("variant aggregate contains multiple variant parts".into());
                    }
                    part = match self.variant_part(
                        &child,
                        unit_index,
                        reference.id,
                        record_kind,
                        &mut budget,
                    ) {
                        Ok(part) => Some(part),
                        Err(VariantMetadataError::Malformed(reason)) => return Err(reason),
                        Err(VariantMetadataError::Limit) => return Ok(limit()),
                    };
                }
                tag if is_scope_only_child(tag) => {}
                tag => {
                    return Ok(opaque(
                        reference,
                        name,
                        explicit_size,
                        format!("variant aggregate contains unsupported direct child {tag:?}"),
                    ));
                }
            }
        }
        let (discriminant, variants) = part.ok_or("variant aggregate has no variant part")?;
        Ok(resolved(
            reference,
            name,
            explicit_size,
            TypeKind::Variant {
                storage,
                common_members: common_members.into(),
                bases: bases.into(),
                discriminant: Box::new(discriminant),
                variants: variants.into(),
                incomplete,
            },
        ))
    }

    /// A variant part's discriminant and variants.
    #[expect(
        clippy::too_many_lines,
        reason = "a variant part's discriminator and arms validate one nested DIE contract"
    )]
    fn variant_part(
        &mut self,
        part: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        aggregate: TypeId,
        record_kind: RecordKind,
        budget: &mut VariantMetadataBudget,
    ) -> std::result::Result<(VariantDiscriminant, Vec<Variant>), VariantMetadataError> {
        let discriminator = die_reference_with_signatures(
            part.attr_value(gimli::DW_AT_discr),
            unit_index,
            self.units,
            self.type_signatures,
        )
        .map_err(malformed)?;
        let tag_type = self.target(part, unit_index)?;
        let discriminant = if let Some(discriminator) = discriminator {
            let mut stored = None;
            for child in self.children(unit_index, part.offset())? {
                let child = child?;
                if child.offset().0 != discriminator.offset || discriminator.unit != unit_index {
                    continue;
                }
                if child.tag() != gimli::DW_TAG_member {
                    return Err(VariantMetadataError::Malformed(
                        "DW_AT_discr does not reference a member child".into(),
                    ));
                }
                budget.consume()?;
                stored = Some(self.build_member(
                    &child,
                    unit_index,
                    aggregate,
                    DynamicAggregateChild::Discriminant,
                    AggregateMemberPath::Discriminant,
                    None,
                    record_kind,
                    "variant component",
                )?);
            }
            let stored = stored
                .ok_or_else(|| Arc::from("DW_AT_discr references a non-child discriminator"))?;
            if let Some(tag_type) = tag_type
                && tag_type.id != stored.type_ref.id
            {
                return Err(VariantMetadataError::Malformed(
                    "variant tag type differs from its discriminator member".into(),
                ));
            }
            VariantDiscriminant::Stored(stored)
        } else {
            tag_type.map_or(VariantDiscriminant::Absent, VariantDiscriminant::TagType)
        };
        let representation = match &discriminant {
            VariantDiscriminant::Stored(member) => Some(member.type_ref.id),
            VariantDiscriminant::TagType(tag_type) => Some(tag_type.id),
            VariantDiscriminant::Absent => None,
        }
        .map(|id| self.resolved_integer_base(id))
        .transpose()?;

        let unit = &self.units[unit_index];
        let mut variants = Vec::new();
        for variant in self.children(unit_index, part.offset())? {
            let variant = variant?;
            if variant.tag() == gimli::DW_TAG_member {
                continue;
            }
            if variant.tag() != gimli::DW_TAG_variant {
                return Err(VariantMetadataError::Malformed(
                    format!(
                        "variant part contains unsupported direct child {:?}",
                        variant.tag()
                    )
                    .into(),
                ));
            }
            budget.consume()?;
            let selection = match &representation {
                Some(representation) => {
                    copy_variant_selection(&variant, representation, self.byte_order, budget)?
                }
                None if variant.attr_value(gimli::DW_AT_discr_value).is_some()
                    || variant.attr_value(gimli::DW_AT_discr_list).is_some() =>
                {
                    return Err(VariantMetadataError::Malformed(
                        "a variant selects a discriminant its part does not have".into(),
                    ));
                }
                None => VariantSelection::Default,
            };
            let name = copy_name(self.dwarf, unit, &variant).map_err(malformed)?;
            let variant_index = variants.len();
            let mut members = Vec::new();
            for member in self.children(unit_index, variant.offset())? {
                let member = member?;
                if member.tag() != gimli::DW_TAG_member {
                    return Err(VariantMetadataError::Malformed(
                        format!("variant contains unsupported component {:?}", member.tag()).into(),
                    ));
                }
                budget.consume()?;
                let member_index = members.len();
                members.push(self.build_member(
                    &member,
                    unit_index,
                    aggregate,
                    DynamicAggregateChild::VariantMember {
                        variant: variant_index,
                        member: member_index,
                    },
                    AggregateMemberPath::Variant {
                        variant: variant_index,
                        member: member_index,
                    },
                    None,
                    record_kind,
                    "variant component",
                )?);
            }
            variants.push(Variant {
                name,
                selection,
                members: members.into(),
            });
        }
        // A sum with no stored tag selects among its variants by which can
        // hold a value, not by selectors; one with none holds no value.
        if !matches!(discriminant, VariantDiscriminant::Absent) {
            if variants.is_empty() {
                return Err(VariantMetadataError::Malformed(
                    "variant part has no variants".into(),
                ));
            }
            validate_variant_selections(&variants)?;
        }
        Ok((discriminant, variants))
    }

    /// Records whether calls pass a C++ class by value, when its producer
    /// says.
    fn note_calling_convention(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        id: TypeId,
    ) {
        match entry.attr_value(gimli::DW_AT_calling_convention) {
            Some(gimli::AttributeValue::CallingConvention(gimli::DW_CC_pass_by_value)) => {
                self.passed_by_value.insert(id, true);
            }
            Some(gimli::AttributeValue::CallingConvention(gimli::DW_CC_pass_by_reference)) => {
                self.passed_by_value.insert(id, false);
            }
            _ => {}
        }
    }

    /// The size of a Zig packed struct that Zig's own backend describes by
    /// the integer that backs it.
    fn packed_size(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> std::result::Result<Option<u64>, Arc<str>> {
        if self.language(unit_index) != SourceLanguage::Zig {
            return Ok(None);
        }
        Ok(self
            .target(entry, unit_index)?
            .and_then(|backing| self.byte_size_of(backing.id)))
    }

    fn build_record_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
    ) -> Built {
        let kind = if entry.tag() == gimli::DW_TAG_class_type {
            RecordKind::Class
        } else {
            RecordKind::Struct
        };
        let incomplete = strict_flag(entry, gimli::DW_AT_declaration)?;
        let explicit_size = match explicit_size {
            None if !incomplete => self.packed_size(entry, unit_index)?,
            size => size,
        };
        if !incomplete && explicit_size.is_none() {
            return Err("complete record type has no byte size".into());
        }
        self.note_calling_convention(entry, reference.id);
        if self.has_direct_variant_part(entry, unit_index)? {
            let storage = if kind == RecordKind::Class {
                VariantStorageKind::Class
            } else {
                VariantStorageKind::Struct
            };
            return self.build_variant_type(
                entry,
                unit_index,
                reference,
                explicit_name,
                explicit_size,
                storage,
            );
        }
        let name = explicit_name.unwrap_or_else(|| {
            Arc::from(format!("<anonymous {:?}@0x{:x}>", kind, entry.offset().0))
        });
        let mut members = Vec::new();
        let mut bases = Vec::new();
        for child in self.children(unit_index, entry.offset())? {
            let child = child?;
            match child.tag() {
                // A DWARF 4 static data member is a declaration with no bytes
                // in an instance.
                gimli::DW_TAG_member
                    if strict_flag(&child, gimli::DW_AT_declaration).unwrap_or(false) => {}
                gimli::DW_TAG_member | gimli::DW_TAG_inheritance
                    if members.len().saturating_add(bases.len()) >= MAX_RECORD_CHILDREN =>
                {
                    return Err("record child count exceeds its limit".into());
                }
                gimli::DW_TAG_member => {
                    let index = members.len();
                    members.push(self.build_member(
                        &child,
                        unit_index,
                        reference.id,
                        DynamicAggregateChild::Member(index),
                        AggregateMemberPath::Direct(index),
                        None,
                        kind,
                        "record member",
                    )?);
                }
                gimli::DW_TAG_inheritance => {
                    bases.push(self.build_base(
                        &child,
                        unit_index,
                        reference.id,
                        bases.len(),
                        kind,
                        "base class",
                    )?);
                }
                tag if is_scope_only_child(tag) => {}
                tag => {
                    return Ok(opaque(
                        reference,
                        name,
                        explicit_size,
                        format!("record contains unsupported direct child {tag:?}"),
                    ));
                }
            }
        }
        let normalized = self
            .normalize_zig_optional_or_error_union(
                unit_index,
                reference.id,
                &name,
                &members,
                incomplete,
            )
            .or_else(|| {
                self.normalize_zig_tagged_union(unit_index, reference.id, &members, incomplete)
            });
        let kind = match normalized {
            Some(kind) => kind?,
            None => TypeKind::Record {
                kind,
                members: members.into(),
                bases: bases.into(),
                incomplete,
            },
        };
        Ok(resolved(reference, name, explicit_size, kind))
    }

    /// A self-hosted Zig optional pointer, `?*T` or `?[*]T`, as the pointer
    /// it is: a union of a pointer's size whose discriminant is the
    /// pointer's own bits, `null` when they are 0, and whose other variant
    /// is the pointer, as the LLVM backend describes it. Any other type is
    /// itself.
    fn zig_nullable_pointer(&self, info: TypeInfo) -> TypeInfo {
        let info_of = |reference: TypeReference| match self.entries.get(reference.id.index()) {
            Some(TypeEntry::Resolved(info)) => Some(info),
            _ => None,
        };
        let TypeKind::Variant {
            common_members,
            bases,
            discriminant,
            variants,
            ..
        } = &info.kind
        else {
            return info;
        };
        let VariantDiscriminant::Stored(discriminant) = discriminant.as_ref() else {
            return info;
        };
        let whole = |member: &RecordMember| {
            member.layout == RecordMemberLayout::ByteOffset(0)
                && info_of(member.type_ref).and_then(|member| member.byte_size) == info.byte_size
        };
        let pointer = match variants.as_ref() {
            [null, some]
                if info.name.starts_with('?')
                    && common_members.is_empty()
                    && bases.is_empty()
                    && whole(discriminant)
                    && null.selection
                        == VariantSelection::Selectors(Arc::from([VariantSelector::Value(
                            IntegerValue::Unsigned(0),
                        )]))
                    && some.selection == VariantSelection::Default =>
            {
                match some.members.as_ref() {
                    [payload] if whole(payload) => info_of(payload.type_ref)
                        .filter(|payload| matches!(payload.kind, TypeKind::Pointer { .. })),
                    _ => None,
                }
            }
            _ => None,
        };
        match pointer {
            Some(pointer) => TypeInfo {
                kind: pointer.kind.clone(),
                ..info
            },
            None => info,
        }
    }

    fn build_union_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
    ) -> Built {
        let incomplete = strict_flag(entry, gimli::DW_AT_declaration)?;
        if !incomplete && explicit_size.is_none() {
            return Err("complete union type has no byte size".into());
        }
        if self.has_direct_variant_part(entry, unit_index)? {
            let built = self.build_variant_type(
                entry,
                unit_index,
                reference,
                explicit_name,
                explicit_size,
                VariantStorageKind::Union,
            )?;
            return Ok(match built {
                TypeEntry::Resolved(info) if self.is_zig(unit_index) => {
                    TypeEntry::Resolved(self.zig_nullable_pointer(info))
                }
                built => built,
            });
        }
        let name = explicit_name
            .unwrap_or_else(|| Arc::from(format!("<anonymous union@0x{:x}>", entry.offset().0)));
        let mut members = Vec::new();
        for child in self.children(unit_index, entry.offset())? {
            let child = child?;
            match child.tag() {
                gimli::DW_TAG_member
                    if strict_flag(&child, gimli::DW_AT_declaration).unwrap_or(false) => {}
                gimli::DW_TAG_member if members.len() >= MAX_RECORD_CHILDREN => {
                    return Ok(opaque(
                        reference,
                        name,
                        explicit_size,
                        "union member count exceeds its resource limit",
                    ));
                }
                gimli::DW_TAG_member => {
                    let index = members.len();
                    members.push(self.build_member(
                        &child,
                        unit_index,
                        reference.id,
                        DynamicAggregateChild::Member(index),
                        AggregateMemberPath::Direct(index),
                        Some(0),
                        RecordKind::Struct,
                        "union member",
                    )?);
                }
                tag if is_scope_only_child(tag) => {}
                tag => {
                    return Ok(opaque(
                        reference,
                        name,
                        explicit_size,
                        format!("union contains unsupported direct child {tag:?}"),
                    ));
                }
            }
        }
        Ok(resolved(
            reference,
            name,
            explicit_size,
            TypeKind::Union {
                members: members.into(),
                incomplete,
            },
        ))
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
                    .or_else(|| self.byte_size_of(target.id))
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
        // A member placed by its first bit alone, as Zig's own backend
        // places a packed struct's, spans its type's bits.
        if let Some(bit_offset) = entry
            .attr(gimli::DW_AT_data_bit_offset)
            .and_then(gimli::Attribute::udata_value)
        {
            if bit_offset % 8 == 0 {
                return Ok(RecordMemberLayout::ByteOffset(bit_offset / 8));
            }
            let bit_size = match self.entries.get(target.id.index()) {
                Some(TypeEntry::Resolved(TypeInfo {
                    kind: TypeKind::Base(base),
                    ..
                })) => base.bit_size.or_else(|| base.byte_size.checked_mul(8)),
                _ => None,
            }
            .ok_or_else(|| Arc::from("member placed by bit has no bit width"))?;
            bit_offset
                .checked_add(bit_size)
                .ok_or_else(|| Arc::from("record bit-field range overflows"))?;
            return Ok(RecordMemberLayout::BitRange {
                bit_offset,
                bit_size,
            });
        }
        Ok(entry.attr(gimli::DW_AT_data_member_location).map_or_else(
            || {
                absent_byte_offset
                    .map_or(RecordMemberLayout::Runtime, RecordMemberLayout::ByteOffset)
            },
            |attribute| {
                constant_member_offset(attribute)
                    .map_or(RecordMemberLayout::Runtime, RecordMemberLayout::ByteOffset)
            },
        ))
    }

    fn build_array_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
    ) -> Built {
        let element = self
            .target(entry, unit_index)?
            .ok_or("array type has no element type")?;
        let unit = &self.units[unit_index];
        let mut dimensions = Vec::new();
        let mut strided = has_stride(entry);
        for child in self.children(unit_index, entry.offset())? {
            let child = child?;
            if child.tag() != gimli::DW_TAG_subrange_type {
                continue;
            }
            strided |= has_stride(&child);
            let signed_index = index_type_is_signed(unit, &child);
            let lower = child
                .attr(gimli::DW_AT_lower_bound)
                .and_then(|attribute| array_bound(attribute, signed_index))
                .unwrap_or(0);
            let count = child
                .attr(gimli::DW_AT_count)
                .and_then(gimli::Attribute::udata_value)
                .or_else(|| {
                    let upper = array_bound(child.attr(gimli::DW_AT_upper_bound)?, signed_index)?;
                    u64::try_from(upper.checked_sub(lower)?.checked_add(1)?).ok()
                });
            let Some(count) = count else {
                return Ok(opaque(
                    reference,
                    explicit_name.unwrap_or_else(|| Arc::from("<dynamic array>")),
                    explicit_size,
                    "array bounds are dynamic or missing",
                ));
            };
            dimensions.push(ArrayDimension {
                lower_bound: lower,
                count,
            });
        }
        if dimensions.is_empty() {
            return Err("array type has no subrange dimensions".into());
        }
        let name =
            explicit_name.unwrap_or_else(|| Arc::from(format!("{}[]", self.target_name(element))));
        // Producers rarely give a C array a size of its own: it is its
        // elements', laid end to end unless a stride spaces them.
        let byte_size = explicit_size.or_else(|| {
            let element_size = self.byte_size_of(element.id)?;
            if strided {
                return None;
            }
            dimensions.iter().try_fold(element_size, |size, dimension| {
                size.checked_mul(dimension.count)
            })
        });
        Ok(resolved(
            reference,
            name,
            byte_size,
            TypeKind::Array {
                element,
                dimensions: dimensions.into(),
            },
        ))
    }

    fn build_slice_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
        layout: SliceLayout,
    ) -> Built {
        let name = explicit_name.unwrap_or_else(|| Arc::from("<slice>"));
        let byte_size = explicit_size.ok_or("slice descriptor has no byte size")?;
        let unit = &self.units[unit_index];
        let address_size = u64::from(unit.encoding().address_size);
        let field_names = match layout {
            SliceLayout::Rust | SliceLayout::RustBytes(_) => &["data_ptr", "length"][..],
            SliceLayout::Zig => &["ptr", "len"][..],
            SliceLayout::Go => &["array", "len", "cap"][..],
        };
        let field_count = u64::try_from(field_names.len()).expect("slice field count fits u64");
        let word_size = byte_size
            .checked_div(field_count)
            .ok_or("slice descriptor size is invalid")?;
        if byte_size != word_size * field_count || word_size != address_size {
            return Ok(opaque(
                reference,
                name,
                Some(byte_size),
                "slice descriptor does not use target-sized words",
            ));
        }
        let mut fields = Vec::new();
        for child in self.children(unit_index, entry.offset())? {
            let child = child?;
            if child.tag() != gimli::DW_TAG_member {
                continue;
            }
            let field_name = copy_name(self.dwarf, unit, &child)
                .map_err(malformed)?
                .ok_or("slice member has no name")?;
            let offset = child
                .attr(gimli::DW_AT_data_member_location)
                .and_then(gimli::Attribute::udata_value)
                .ok_or("slice member has no constant offset")?;
            let field_type = self
                .target(&child, unit_index)?
                .ok_or("slice member has no type")?;
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
            return Ok(opaque(
                reference,
                name,
                Some(byte_size),
                "unrecognized slice descriptor layout",
            ));
        }
        let element = self.slice_element(layout, fields[0].2)?;
        for (_, _, field_type) in &fields[1..] {
            let valid = self
                .entries
                .get(field_type.id.index())
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
                return Err(
                    "slice length and capacity members must be target-sized unsigned integers"
                        .into(),
                );
            }
        }
        let text = layout.is_text(&name);
        Ok(resolved(
            reference,
            name,
            Some(byte_size),
            TypeKind::Slice {
                element,
                has_capacity: layout == SliceLayout::Go,
                text,
            },
        ))
    }
}

impl TypeArenaBuilder<'_, '_> {
    /// A slice's element type: what its data member points to, or a string
    /// type's bytes.
    fn slice_element(
        &mut self,
        layout: SliceLayout,
        data: TypeReference,
    ) -> std::result::Result<TypeReference, Arc<str>> {
        if let SliceLayout::RustBytes(bytes) = layout {
            let array = self.units[bytes.unit]
                .entry(gimli::UnitOffset(bytes.offset))
                .map_err(malformed)?;
            return self
                .type_reference(bytes.unit, array.attr_value(gimli::DW_AT_type))?
                .ok_or_else(|| "string type's bytes have no type".into());
        }
        match self.entries.get(data.id.index()) {
            Some(TypeEntry::Resolved(TypeInfo {
                kind:
                    TypeKind::Pointer {
                        target: Some(target),
                        ..
                    },
                ..
            })) => Ok(*target),
            _ => Err("slice data member is not a typed pointer".into()),
        }
    }
}

/// Which language's slice descriptor a structure is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SliceLayout {
    /// `{data_ptr, length}`: a pointer to a slice or `str`.
    Rust,
    /// `{data_ptr, length}`: a pointer to a string type, such as `Path`,
    /// `OsStr`, or `CStr`, which wraps the unsized array of bytes it holds.
    RustBytes(DieKey),
    /// `{ptr, len}`.
    Zig,
    /// `{array, len, cap}`.
    Go,
}

impl SliceLayout {
    /// Whether a slice so named is the language's text: Rust's `str`, and
    /// Zig's `[]const u8` and its sentinel-terminated forms.
    fn is_text(self, name: &str) -> bool {
        match self {
            Self::Rust => {
                matches!(name, "&str" | "&mut str" | "*const str" | "*mut str")
                    || name
                        .strip_prefix("alloc::boxed::Box<str")
                        .is_some_and(|rest| rest.starts_with([',', '>']))
            }
            Self::RustBytes(_) => true,
            Self::Zig => matches!(name, "[]const u8" | "[:0]const u8" | "[:0]u8"),
            Self::Go => false,
        }
    }
}

impl<'data> TypeArenaBuilder<'_, 'data> {
    /// Which language's slice descriptor a structure DIE is, judged by its
    /// shape and by what its language says, never by a library's names.
    ///
    /// rustc emits each pointer to a slice or `str` as a structure of a data
    /// pointer and a length outside every module, whatever it names it:
    /// `&[T]`, `*const [T]` when optimizing, `&mut [T]`, `Box<[T]>`. A
    /// pointer to a type with an unsized tail, such as `&Path`, has the same
    /// shape, but its length counts the tail, so it is not a slice, unless
    /// the tail is bytes that wrappers alone hold, as in `Path`. Go marks
    /// slices by kind, and Zig spells them `[]T` and `[:s]T`.
    fn slice_layout(
        &self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        key: DieKey,
        name: Option<&str>,
    ) -> Option<SliceLayout> {
        match self.language(key.unit) {
            SourceLanguage::Go => Self::go_kind(entry)
                .map_or_else(
                    || name.is_some_and(|name| name.starts_with("[]")),
                    |kind| kind == GoKind::Slice,
                )
                .then_some(SliceLayout::Go),
            SourceLanguage::Zig => name
                .is_some_and(|name| name.starts_with("[]") || name.starts_with("[:"))
                .then_some(SliceLayout::Zig),
            SourceLanguage::Rust if !self.type_scopes.contains_key(&key) => {
                let members = self.member_types(entry, key.unit)?;
                let [(first, data), (second, _)] = members.as_slice() else {
                    return None;
                };
                if first.as_ref() != "data_ptr" || second.as_ref() != "length" {
                    return None;
                }
                let data = (*data)?;
                if !self.points_to_unsized(data) {
                    return Some(SliceLayout::Rust);
                }
                self.string_bytes(data).map(SliceLayout::RustBytes)
            }
            _ => None,
        }
    }

    /// A structure's members' names and type DIEs, in order.
    fn member_types(
        &self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> Option<Vec<(Arc<str>, Option<DieKey>)>> {
        let unit = self.units.get(unit_index)?;
        let mut members = Vec::new();
        for child in self.children(unit_index, entry.offset()).ok()? {
            let child = child.ok()?;
            if child.tag() != gimli::DW_TAG_member {
                continue;
            }
            if members.len() >= MAX_RECORD_CHILDREN {
                return None;
            }
            let name = copy_name(self.dwarf, unit, &child).ok()??;
            let target = die_reference_with_signatures(
                child.attr_value(gimli::DW_AT_type),
                unit_index,
                self.units,
                self.type_signatures,
            )
            .ok()?;
            members.push((name, target));
        }
        Some(members)
    }

    /// Whether a pointer DIE points to a type whose last member is unsized,
    /// such as `Path` or `RcInner<str>`. Anything unreadable counts as
    /// unsized, so that a doubtful pointer is never presented as a slice.
    fn points_to_unsized(&self, pointer: DieKey) -> bool {
        const MAX_DEPTH: usize = 16;
        let target = |key: DieKey| -> Option<(
            gimli::DebuggingInformationEntry<Reader<'data>>,
            Option<DieKey>,
        )> {
            let unit = self.units.get(key.unit)?;
            let entry = unit.entry(gimli::UnitOffset(key.offset)).ok()?;
            let target = die_reference_with_signatures(
                entry.attr_value(gimli::DW_AT_type),
                key.unit,
                self.units,
                self.type_signatures,
            )
            .ok()?;
            Some((entry, target))
        };
        let Some((pointer_entry, Some(mut current))) = target(pointer) else {
            return true;
        };
        if pointer_entry.tag() != gimli::DW_TAG_pointer_type {
            return true;
        }
        for _ in 0..MAX_DEPTH {
            let Some((entry, next)) = target(current) else {
                return true;
            };
            match entry.tag() {
                gimli::DW_TAG_typedef | gimli::DW_TAG_const_type | gimli::DW_TAG_volatile_type => {
                    match next {
                        Some(next) => current = next,
                        None => return false,
                    }
                }
                gimli::DW_TAG_array_type => return self.array_is_unsized(current),
                gimli::DW_TAG_structure_type => {
                    let Some(members) = self.member_types(&entry, current.unit) else {
                        return true;
                    };
                    match members.last() {
                        Some((_, Some(last))) => current = *last,
                        Some((_, None)) => return true,
                        None => return false,
                    }
                }
                _ => return false,
            }
        }
        true
    }

    /// The unsized array of bytes a pointer DIE's target holds, when that
    /// target is records each of a single member at offset zero, ending in
    /// the array, as `Path`, `OsStr`, and `CStr` are.
    fn string_bytes(&self, pointer: DieKey) -> Option<DieKey> {
        const MAX_DEPTH: usize = 16;
        let entry = |key: DieKey| {
            self.units
                .get(key.unit)?
                .entry(gimli::UnitOffset(key.offset))
                .ok()
        };
        let target = |entry: &gimli::DebuggingInformationEntry<Reader<'data>>, unit| {
            die_reference_with_signatures(
                entry.attr_value(gimli::DW_AT_type),
                unit,
                self.units,
                self.type_signatures,
            )
            .ok()
            .flatten()
        };
        let pointer_entry = entry(pointer)?;
        let mut current = target(&pointer_entry, pointer.unit)?;
        let mut wrapped = false;
        for _ in 0..MAX_DEPTH {
            let current_entry = entry(current)?;
            match current_entry.tag() {
                gimli::DW_TAG_structure_type => {
                    let mut members = self
                        .children(current.unit, current_entry.offset())
                        .ok()?
                        .map_while(Result::ok)
                        .filter(|child| child.tag() == gimli::DW_TAG_member);
                    let member = members.next()?;
                    let at_start = member
                        .attr(gimli::DW_AT_data_member_location)
                        .and_then(gimli::Attribute::udata_value)
                        == Some(0);
                    if members.next().is_some() || !at_start {
                        return None;
                    }
                    current = target(&member, current.unit)?;
                    wrapped = true;
                }
                gimli::DW_TAG_array_type if wrapped && self.array_is_unsized(current) => {
                    let element = entry(target(&current_entry, current.unit)?)?;
                    let byte = element.tag() == gimli::DW_TAG_base_type
                        && element
                            .attr(gimli::DW_AT_byte_size)
                            .and_then(gimli::Attribute::udata_value)
                            == Some(1);
                    return byte.then_some(current);
                }
                _ => return None,
            }
        }
        None
    }

    /// Whether an array type DIE has a dimension with no count.
    pub(super) fn array_is_unsized(&self, array: DieKey) -> bool {
        let Ok(children) = self.children(array.unit, gimli::UnitOffset(array.offset)) else {
            return true;
        };
        children.map_while(Result::ok).any(|child| {
            child.tag() == gimli::DW_TAG_subrange_type
                && child.attr(gimli::DW_AT_count).is_none()
                && child.attr(gimli::DW_AT_upper_bound).is_none()
        })
    }
}

/// Resolves a type DIE's `DW_AT_address_class`, which defaults to zero. A
/// class too large to represent is unsupported, and any other non-constant
/// form defective: either read as the default class would use semantics the
/// producer never specified.
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
        UnsignedConstant::Oversized => Err(Box::new(opaque(
            reference,
            explicit_name.unwrap_or_else(|| Arc::from("<unsupported type>")),
            None,
            "DW_AT_address_class exceeds the supported u64 range",
        ))),
        UnsignedConstant::NonConstant => Err(Box::new(TypeEntry::Malformed(
            "DW_AT_address_class is not an unsigned integer constant".into(),
        ))),
    }
}

/// Resolves a type DIE's `DW_AT_byte_size`: `None` when it is absent, and
/// otherwise the size or the terminal entry for a size that is unusable or
/// defective. Only an absent size may fall back to a default.
fn resolve_explicit_size(
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    reference: TypeReference,
    explicit_name: Option<Arc<str>>,
) -> std::result::Result<Option<u64>, Box<TypeEntry>> {
    match byte_size_attribute(entry) {
        ByteSize::Absent => Ok(None),
        ByteSize::Constant(size) => Ok(Some(size)),
        ByteSize::Unsupported(description) => Err(Box::new(opaque(
            reference,
            explicit_name.unwrap_or_else(|| Arc::from("<oversized type>")),
            None,
            description,
        ))),
        ByteSize::Dynamic => Err(Box::new(opaque(
            reference,
            explicit_name.unwrap_or_else(|| Arc::from("<dynamically sized type>")),
            None,
            "dynamic DW_AT_byte_size is unsupported",
        ))),
        ByteSize::Malformed => Err(Box::new(TypeEntry::Malformed(
            "DW_AT_byte_size is neither a constant nor a supported dynamic form".into(),
        ))),
    }
}

/// A type built, or why its metadata is malformed.
type Built = std::result::Result<TypeEntry, Arc<str>>;

fn malformed(error: impl std::fmt::Display) -> Arc<str> {
    error.to_string().into()
}

/// A type with no identity yet.
const fn resolved(
    reference: TypeReference,
    name: Arc<str>,
    byte_size: Option<u64>,
    kind: TypeKind,
) -> TypeEntry {
    TypeEntry::Resolved(TypeInfo {
        reference,
        name,
        byte_size,
        kind,
        identity: None,
    })
}

/// A type whose representation is unsupported.
fn opaque(
    reference: TypeReference,
    name: Arc<str>,
    byte_size: Option<u64>,
    description: impl Into<Arc<str>>,
) -> TypeEntry {
    resolved(
        reference,
        name,
        byte_size,
        TypeKind::Opaque {
            description: description.into(),
        },
    )
}

/// Go's `DW_AT_go_dict_index`: which entry of a generic function's
/// dictionary holds a type parameter's argument.
const DW_AT_GO_DICT_INDEX: gimli::DwAt = gimli::DwAt(0x2906);

/// Whether an encoding is a float or a pair of them.
const fn is_floating(encoding: BaseTypeEncoding) -> bool {
    matches!(
        encoding,
        BaseTypeEncoding::Floating | BaseTypeEncoding::ComplexFloating
    )
}

/// The integral `BaseTypeEncoding` of a `DW_ATE_*` encoding.
const fn integer_encoding(encoding: gimli::DwAte) -> Option<BaseTypeEncoding> {
    Some(match encoding {
        gimli::DW_ATE_boolean => BaseTypeEncoding::Boolean,
        gimli::DW_ATE_signed => BaseTypeEncoding::Signed,
        gimli::DW_ATE_signed_char => BaseTypeEncoding::SignedCharacter,
        gimli::DW_ATE_unsigned => BaseTypeEncoding::Unsigned,
        gimli::DW_ATE_unsigned_char => BaseTypeEncoding::UnsignedCharacter,
        _ => return None,
    })
}

/// The direct children of one DIE, read one at a time.
pub(super) struct Children<'a, 'data> {
    cursor: gimli::EntriesCursor<'a, Reader<'data>>,
    started: bool,
    done: bool,
}

impl<'data> Iterator for Children<'_, 'data> {
    type Item = std::result::Result<gimli::DebuggingInformationEntry<Reader<'data>>, Arc<str>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let next = if self.started {
            self.cursor.next_sibling().map(Option::<&_>::cloned)
        } else {
            self.started = true;
            self.first_child()
        };
        match next {
            Ok(Some(child)) => Some(Ok(child)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(error) => {
                self.done = true;
                Some(Err(malformed(error)))
            }
        }
    }
}

impl<'data> Children<'_, 'data> {
    fn first_child(
        &mut self,
    ) -> gimli::Result<Option<gimli::DebuggingInformationEntry<Reader<'data>>>> {
        let has_children = self
            .cursor
            .next_dfs()?
            .is_some_and(gimli::DebuggingInformationEntry::has_children);
        if !has_children || !self.cursor.next_entry()? {
            return Ok(None);
        }
        Ok(self.cursor.current().cloned())
    }
}

/// Whether a type DIE's `DW_AT_type` must be present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetRequirement {
    Required,
    Optional,
}

fn named_type_relationship(tag: gimli::DwTag, language: SourceLanguage) -> NamedTypeRelationship {
    if tag == gimli::DW_TAG_template_alias {
        return NamedTypeRelationship::Synonym;
    }
    match language {
        SourceLanguage::C | SourceLanguage::Cpp => NamedTypeRelationship::Synonym,
        SourceLanguage::Go => NamedTypeRelationship::Distinct,
        SourceLanguage::Zig => NamedTypeRelationship::Encoding,
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
pub(super) const fn is_type_die_tag(tag: gimli::DwTag) -> bool {
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

pub(super) trait TypeMetadataEntry {
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

pub(super) fn type_info_from<T: TypeMetadataEntry>(
    types: &[T],
    id: TypeId,
) -> std::result::Result<&TypeInfo, Arc<str>> {
    types.get(id.index()).map_or_else(
        || Err("type ID is outside the module arena".into()),
        TypeMetadataEntry::type_info,
    )
}

pub(super) fn propagate_wrapper_sizes(types: &mut [TypeEntry]) {
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
        if let Some(target_dependents) = dependents.get_mut(target.id.index()) {
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

pub(super) fn inline_storage_cycle_nodes(types: &[TypeEntry]) -> Vec<usize> {
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
        targets.push(reference.id.index());
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
        TypeKind::Signature {
            returns,
            parameters,
            ..
        } => {
            if let Some(returns) = returns {
                push(*returns);
            }
            for parameter in parameters.iter() {
                push(*parameter);
            }
        }
        TypeKind::Base(_)
        | TypeKind::Function
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

/// The word a qualifier is written with, or `None` for `_Atomic`, which
/// wraps its type instead.
const fn modifier_keyword(modifier: TypeModifier) -> Option<&'static str> {
    Some(match modifier {
        TypeModifier::Const => "const",
        TypeModifier::Volatile => "volatile",
        TypeModifier::Restrict => "restrict",
        TypeModifier::Immutable => "immutable",
        TypeModifier::Packed => "packed",
        TypeModifier::Shared => "shared",
        TypeModifier::Atomic => return None,
    })
}

fn modifier_type_name(modifier: TypeModifier, target: &str, indirection: bool) -> String {
    let Some(keyword) = modifier_keyword(modifier) else {
        return format!("_Atomic({target})");
    };
    if indirection {
        format!("{target} {keyword}")
    } else {
        format!("{keyword} {target}")
    }
}

/// Whether an array or one of its dimensions spaces its elements apart.
fn has_stride(entry: &gimli::DebuggingInformationEntry<Reader<'_>>) -> bool {
    entry.attr(gimli::DW_AT_byte_stride).is_some() || entry.attr(gimli::DW_AT_bit_stride).is_some()
}
