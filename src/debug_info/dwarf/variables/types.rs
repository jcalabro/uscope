//! Normalizing DWARF type DIEs into the platform-neutral type graph.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use crate::debug_info::dwarf::{
    DieKey, DwarfError, Reader, TypeSignatures, die_reference_with_signatures,
};
use crate::model::ArrayDimension;
use crate::{
    Accessibility, BaseClass, BaseClassVirtuality, BaseType, BaseTypeEncoding, ByteOrder,
    EnumerationOrigin, Enumerator, GoKind, IntegerValue, ModuleImageId, NamedTypeRelationship,
    RecordKind, RecordMember, RecordMemberLayout, ReferenceKind, SourceFile, SourceFileId,
    SourceLanguage, SourceLocation, TypeId, TypeInfo, TypeKind, TypeModifier, TypeReference,
    Variant, VariantDiscriminant, VariantSelection, VariantSelector, VariantStorageKind,
};

use super::codec::enumeration_constant;
use super::die::{
    ByteSize, UnsignedConstant, array_bound, base_type_encoding, byte_size_attribute,
    constant_member_offset, copy_name, copy_name_with_origins, declaration_with_origins,
    index_type_is_signed, origin_chain, strict_flag, unsigned_constant,
};
use super::identity::{
    IdentityParts, ScopePath, ScopeSegment, inline_namespace_path, scope_segment, source_language,
};
use super::location::{Expression, copy_expression};
use super::variant::{
    VariantMetadataBudget, VariantMetadataError, copy_variant_selection,
    validate_variant_selections, variant_metadata_limit_type,
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
    /// DIE-boundary offsets per unit, indexed by unit position. A `DW_AT_type`
    /// offset that is not in its unit's set points into the middle of a DIE and
    /// is defective. Built once so target validation stays O(1) per reference.
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
pub(super) const fn is_scope_only_child(tag: gimli::DwTag) -> bool {
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

    /// Builds the type an attribute refers to, if any, so that the type
    /// index knows it; a malformed reference only goes unbuilt.
    pub(super) fn reach(
        &mut self,
        unit_index: usize,
        value: Option<gimli::AttributeValue<Reader<'data>>>,
    ) {
        if let Ok(Some(key)) =
            die_reference_with_signatures(value, unit_index, self.units, self.type_signatures)
        {
            self.resolve(key);
        }
    }

    pub(super) fn resolve(&mut self, key: DieKey) -> TypeId {
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
            self.entries[id.index()] =
                TypeEntry::Malformed("type wrapper depth exceeds its limit".into());
            return id;
        }
        self.resolution_depth += 1;
        let entry = self.build(key, id);
        self.resolution_depth -= 1;
        self.entries[id.index()] = entry;
        id
    }

    pub(super) fn type_limit(&mut self, key: DieKey) -> TypeId {
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

    /// Returns `void`. Producers omit `DW_AT_type` to mean it, so a
    /// `const void` or `typedef void T` has no target DIE to resolve.
    pub(super) fn void_type(&mut self) -> TypeReference {
        let id = *self.void_type.get_or_insert_with(|| {
            let id = TypeId::new(u32::try_from(self.entries.len()).expect("type count fits u32"));
            self.entries.push(TypeEntry::Resolved(TypeInfo {
                reference: TypeReference {
                    image: self.image,
                    id,
                },
                name: "void".into(),
                byte_size: None,
                kind: TypeKind::Unspecified,
                identity: None,
            }));
            id
        });
        TypeReference {
            image: self.image,
            id,
        }
    }

    pub(super) fn canonical_type_key(&self, key: DieKey) -> std::result::Result<DieKey, Arc<str>> {
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

    pub(super) fn build(&mut self, key: DieKey, id: TypeId) -> TypeEntry {
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
        let named = explicit_name.is_some();
        let slice_layout = if entry.tag() == gimli::DW_TAG_structure_type {
            self.slice_layout(&entry, key, explicit_name.as_deref())
        } else {
            None
        };

        let built = if let Some(layout) = slice_layout {
            self.build_slice_type(
                &entry,
                key.unit,
                reference,
                explicit_name,
                explicit_size,
                layout,
            )
        } else {
            self.build_kind(
                &entry,
                key.unit,
                reference,
                explicit_name,
                explicit_size,
                pointer_size,
                address_class,
            )
        };
        if named && matches!(built, TypeEntry::Resolved(_)) {
            self.record_identity_parts(&entry, key, id);
        }
        built
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
    ) -> TypeEntry {
        let key = DieKey {
            unit: unit_index,
            offset: entry.offset().0,
        };
        match entry.tag() {
            gimli::DW_TAG_base_type => {
                Self::build_base_type(entry, reference, explicit_name, explicit_size)
            }
            gimli::DW_TAG_enumeration_type => self.build_enumeration_type(
                entry,
                key.unit,
                reference,
                explicit_name,
                explicit_size,
            ),
            gimli::DW_TAG_pointer_type => self.build_pointer_type(
                entry,
                key.unit,
                reference,
                explicit_name,
                pointer_size,
                address_class,
            ),
            gimli::DW_TAG_reference_type | gimli::DW_TAG_rvalue_reference_type => self
                .build_reference_type(
                    entry,
                    key.unit,
                    reference,
                    explicit_name,
                    pointer_size,
                    address_class,
                ),
            gimli::DW_TAG_array_type => {
                self.build_array_type(entry, key.unit, reference, explicit_name, explicit_size)
            }
            gimli::DW_TAG_structure_type | gimli::DW_TAG_class_type => {
                self.build_record_type(entry, key.unit, reference, explicit_name, explicit_size)
            }
            gimli::DW_TAG_union_type => {
                self.build_union_type(entry, key.unit, reference, explicit_name, explicit_size)
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
                self.build_wrapper_type(entry, key.unit, reference, explicit_name, explicit_size)
            }
            gimli::DW_TAG_unspecified_type => TypeEntry::Resolved(TypeInfo {
                reference,
                name: explicit_name.unwrap_or_else(|| Arc::from("void")),
                byte_size: explicit_size,
                kind: TypeKind::Unspecified,
                identity: None,
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
                identity: None,
            }),
        }
    }

    pub(super) fn record_accessibility(
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

    pub(super) fn record_byte_layout(
        entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    ) -> RecordMemberLayout {
        entry
            .attr(gimli::DW_AT_data_member_location)
            .and_then(constant_member_offset)
            .map_or(RecordMemberLayout::Runtime, RecordMemberLayout::ByteOffset)
    }

    /// Reports a defect in a tag's mandatory attributes, independent of the byte
    /// size. Validating these before the size classification ensures a dynamic
    /// or oversized size cannot mask a missing encoding or target. Returns `None`
    /// when the tag's required attributes are present and well-formed.
    pub(super) fn mandatory_attribute_defect(
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
    pub(super) fn target_defect(
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

    pub(super) fn build_base_type(
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
                    identity: None,
                });
            }
            // Any other zero-byte base type, such as Rust's unit type `()`,
            // has no bits to decode, whatever its encoding: it holds nothing,
            // as an empty structure does, so normalize it to one.
            return TypeEntry::Resolved(TypeInfo {
                reference,
                name,
                byte_size: Some(0),
                kind: TypeKind::Record {
                    kind: RecordKind::Struct,
                    members: Arc::from([]),
                    bases: Arc::from([]),
                    incomplete: false,
                },
                identity: None,
            });
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
                    identity: None,
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
            identity: None,
        })
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
    pub(super) fn build_enumeration_type(
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
                    identity: None,
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
            identity: None,
        })
    }

    pub(super) fn target(
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

    pub(super) fn target_with_origins(
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
    pub(super) fn populate_go_named_constants(&mut self) {
        let mut constants = BTreeMap::<TypeId, NamedConstantCollection>::new();
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
                let Some(TypeEntry::Resolved(info)) = self.entries.get(target.index()) else {
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

    pub(super) fn target_name(&self, target: TypeReference) -> Arc<str> {
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

    pub(super) fn reject_inline_storage_cycles(&mut self) {
        let description: Arc<str> = "type graph contains an inline-storage cycle".into();
        for node in inline_storage_cycle_nodes(&self.entries) {
            self.entries[node] = TypeEntry::Malformed(Arc::clone(&description));
        }
    }

    /// Renders a type's source-style name from its targets' names. Cycles
    /// and chains deeper than the resolution limit, which only malformed
    /// metadata builds, render as `<type #N>` instead of recursing without
    /// bound on the controller thread's stack.
    pub(super) fn render_type_name(&self, id: TypeId, visiting: &mut HashSet<TypeId>) -> Arc<str> {
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

    pub(super) fn modified_target_is_indirection(&self, start: TypeId) -> bool {
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

    pub(super) fn indirection_type_name(
        &self,
        target: TypeId,
        target_name: &str,
        symbol: &str,
    ) -> String {
        let target_is_synthesized_array = !self.explicit_names.contains(&target)
            && self.entries.get(target.index()).is_some_and(|entry| {
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

    pub(super) fn build_pointer_type(
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
            identity: None,
        })
    }

    pub(super) fn build_reference_type(
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
            identity: None,
        })
    }

    pub(super) fn build_wrapper_type(
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
        // An absent target means `void`, except on a typedef declaration,
        // which is incomplete.
        let declaration = strict_flag(entry, gimli::DW_AT_declaration).unwrap_or(false);
        let target = match target {
            None if !declaration => Some(self.void_type()),
            target => target,
        };
        let inherited_size = target
            .and_then(|target| self.entries.get(target.id.index()))
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
                identity: None,
            });
        }
        // Only a declaration keeps an absent target; a qualifier cannot be one.
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
            identity: None,
        })
    }

    pub(super) fn has_direct_variant_part(
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

    #[expect(
        clippy::too_many_lines,
        reason = "Zig tagged-union recognition and remapping form one fail-closed structural validation"
    )]
    pub(super) fn normalize_zig_tagged_union(
        &mut self,
        unit_index: usize,
        aggregate: TypeId,
        members: &[RecordMember],
        incomplete: bool,
    ) -> Option<std::result::Result<TypeKind, Arc<str>>> {
        if incomplete
            || !self.zig_units.get(unit_index).copied().unwrap_or(false)
            || members.len() != 2
        {
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
    pub(super) fn normalize_zig_optional_or_error_union(
        &mut self,
        unit_index: usize,
        aggregate: TypeId,
        name: &str,
        members: &[RecordMember],
        incomplete: bool,
    ) -> Option<std::result::Result<TypeKind, Arc<str>>> {
        if incomplete
            || !self.zig_units.get(unit_index).copied().unwrap_or(false)
            || members.len() != 2
        {
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
    pub(super) fn build_variant_member(
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
    pub(super) fn build_variant_type(
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
                tag if is_scope_only_child(tag) => {}
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
                        identity: None,
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
            identity: None,
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "record normalization keeps all storage and scope child tags in one auditable dispatch"
    )]
    pub(super) fn build_record_type(
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
                // A DWARF 4 static data member is a declaration with no bytes
                // in an instance.
                gimli::DW_TAG_member
                    if strict_flag(child, gimli::DW_AT_declaration).unwrap_or(false) => {}
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
                        identity: None,
                    });
                }
                tag if is_scope_only_child(tag) => {}
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
                        identity: None,
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
                    identity: None,
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
                    identity: None,
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
            identity: None,
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "union normalization keeps overlapping storage and scope children explicit"
    )]
    pub(super) fn build_union_type(
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
                gimli::DW_TAG_member
                    if strict_flag(child, gimli::DW_AT_declaration).unwrap_or(false) => {}
                gimli::DW_TAG_member => {
                    if members.len() >= MAX_RECORD_CHILDREN {
                        return TypeEntry::Resolved(TypeInfo {
                            reference,
                            name,
                            byte_size: explicit_size,
                            kind: TypeKind::Opaque {
                                description: "union member count exceeds its resource limit".into(),
                            },
                            identity: None,
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
                        identity: None,
                    });
                }
                tag if is_scope_only_child(tag) => {}
                tag => {
                    return TypeEntry::Resolved(TypeInfo {
                        reference,
                        name,
                        byte_size: explicit_size,
                        kind: TypeKind::Opaque {
                            description: format!("union contains unsupported direct child {tag:?}")
                                .into(),
                        },
                        identity: None,
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
            identity: None,
        })
    }

    pub(super) fn record_member_layout(
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
                            .get(target.id.index())
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
                constant_member_offset(attribute)
                    .map_or(RecordMemberLayout::Runtime, RecordMemberLayout::ByteOffset)
            },
        ))
    }

    pub(super) fn copy_dynamic_record_layout(
        &self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> std::result::Result<Option<Expression>, DwarfError> {
        let Some(expression) = entry
            .attr_value(gimli::DW_AT_data_member_location)
            .and_then(|value| value.exprloc_value())
        else {
            return Ok(None);
        };
        let unit = self
            .units
            .get(unit_index)
            .ok_or(DwarfError::ReferenceOutsideUnits(unit_index))?;
        copy_expression(self.dwarf, unit_index, unit, expression, unit.encoding()).map(Some)
    }

    pub(super) fn build_array_type(
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
        let mut strided = has_stride(entry);
        let mut children = root.children();
        loop {
            let child = match children.next() {
                Ok(Some(child)) => child,
                Ok(None) => break,
                Err(error) => return TypeEntry::Malformed(error.to_string().into()),
            };
            if child.entry().tag() != gimli::DW_TAG_subrange_type {
                continue;
            }
            let child = child.entry();
            strided |= has_stride(child);
            let signed_index = index_type_is_signed(unit, child);
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
                return TypeEntry::Resolved(TypeInfo {
                    reference,
                    name: explicit_name.unwrap_or_else(|| Arc::from("<dynamic array>")),
                    byte_size: explicit_size,
                    kind: TypeKind::Opaque {
                        description: "array bounds are dynamic or missing".into(),
                    },
                    identity: None,
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
        // Producers rarely give a C array a size of its own: it is its
        // elements', laid end to end unless a stride spaces them.
        let byte_size = explicit_size.or_else(|| {
            let element_size = match self.entries.get(element.id.index())? {
                TypeEntry::Resolved(info) => info.byte_size?,
                TypeEntry::Building | TypeEntry::Malformed(_) => return None,
            };
            if strided {
                return None;
            }
            dimensions.iter().try_fold(element_size, |size, dimension| {
                size.checked_mul(dimension.count)
            })
        });
        TypeEntry::Resolved(TypeInfo {
            reference,
            name,
            byte_size,
            kind: TypeKind::Array {
                element,
                dimensions: dimensions.into(),
            },
            identity: None,
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "slice normalization validates both compiler layouts and every field invariant"
    )]
    pub(super) fn build_slice_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
        layout: SliceLayout,
    ) -> TypeEntry {
        let name = explicit_name.unwrap_or_else(|| Arc::from("<slice>"));
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
        let address_size = u64::from(unit.encoding().address_size);
        let field_names = match layout {
            SliceLayout::Rust => &["data_ptr", "length"][..],
            SliceLayout::Zig => &["ptr", "len"][..],
            SliceLayout::Go => &["array", "len", "cap"][..],
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
                identity: None,
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
                identity: None,
            });
        }
        let pointer = fields[0].2;
        let element = match self.entries.get(pointer.id.index()) {
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
                return TypeEntry::Malformed(
                    "slice length and capacity members must be target-sized unsigned integers"
                        .into(),
                );
            }
        }
        let text = layout.is_text(&name);
        TypeEntry::Resolved(TypeInfo {
            reference,
            name,
            byte_size: Some(byte_size),
            kind: TypeKind::Slice {
                element,
                has_capacity: layout == SliceLayout::Go,
                text,
            },
            identity: None,
        })
    }
}

/// Which language's slice descriptor a structure is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SliceLayout {
    /// `{data_ptr, length}`: a pointer to a slice or `str`.
    Rust,
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
    /// shape, but its length counts the tail, so it is not a slice. Go marks
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
                (first.as_ref() == "data_ptr"
                    && second.as_ref() == "length"
                    && !data.is_none_or(|data| self.points_to_unsized(data)))
                .then_some(SliceLayout::Rust)
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
        let mut tree = unit.entries_tree(Some(entry.offset())).ok()?;
        let root = tree.root().ok()?;
        let mut children = root.children();
        let mut members = Vec::new();
        while let Some(child) = children.next().ok()? {
            let child = child.entry();
            if child.tag() != gimli::DW_TAG_member {
                continue;
            }
            if members.len() >= MAX_RECORD_CHILDREN {
                return None;
            }
            let name = copy_name(self.dwarf, unit, child).ok()??;
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

    /// Whether an array type DIE has a dimension with no count.
    fn array_is_unsized(&self, array: DieKey) -> bool {
        let Some(unit) = self.units.get(array.unit) else {
            return true;
        };
        let Ok(mut tree) = unit.entries_tree(Some(gimli::UnitOffset(array.offset))) else {
            return true;
        };
        let Ok(root) = tree.root() else {
            return true;
        };
        let mut children = root.children();
        while let Ok(Some(child)) = children.next() {
            let child = child.entry();
            if child.tag() == gimli::DW_TAG_subrange_type
                && child.attr(gimli::DW_AT_count).is_none()
                && child.attr(gimli::DW_AT_upper_bound).is_none()
            {
                return true;
            }
        }
        false
    }
}

/// Resolves a type DIE's `DW_AT_address_class`.
///
/// An absent attribute defaults to zero. A present oversized constant is valid
/// but uninterpretable here (opaque); any other non-constant form is defective.
/// Silently treating either as the default class could produce a convincing read
/// using semantics the producer never specified.
pub(super) fn resolve_address_class(
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
            identity: None,
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
pub(super) fn resolve_explicit_size(
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
            identity: None,
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
            identity: None,
        }))),
        ByteSize::Malformed => Err(Box::new(TypeEntry::Malformed(
            "DW_AT_byte_size is neither a constant nor a supported dynamic form".into(),
        ))),
    }
}

/// Whether a type DIE's `DW_AT_type` edge must be present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TargetRequirement {
    /// The tag mandates a target (e.g. a reference or qualifier).
    Required,
    /// The target is optional (e.g. a `void` pointer), but if present it must
    /// still name a real type DIE.
    Optional,
}

pub(super) fn named_type_relationship(
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

pub(super) const fn type_modifier(tag: gimli::DwTag) -> Option<TypeModifier> {
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

pub(super) fn inline_storage_targets(kind: &TypeKind, targets: &mut Vec<usize>) {
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

pub(super) fn modifier_type_name(
    modifier: TypeModifier,
    target: &str,
    indirection: bool,
) -> String {
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

/// Whether an array or one of its dimensions spaces its elements apart.
fn has_stride(entry: &gimli::DebuggingInformationEntry<Reader<'_>>) -> bool {
    entry.attr(gimli::DW_AT_byte_stride).is_some() || entry.attr(gimli::DW_AT_bit_stride).is_some()
}
