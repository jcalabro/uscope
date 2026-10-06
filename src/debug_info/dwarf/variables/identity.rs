//! Type identities from DWARF: each type's language, the scopes that
//! enclose it, its template parameters, and Go's type attributes.
//!
//! The structure comes from DWARF wherever DWARF has it. Names are parsed
//! only where it does not: GCC omits some templates' parameters, rustc omits
//! const generic parameters, and Go and Zig emit none. A parsed argument
//! names a type only when exactly one type matches it.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::debug_info::dwarf::{DieKey, Reader, die_reference_with_signatures, string_attribute};
use crate::type_identity::{
    ANONYMOUS_NAMESPACE, NameSyntax, TypeIndex, TypeLookup, TypeName, parse_integer,
};
use crate::{
    ArgumentOrigin, GoKind, GoTypeAttributes, SourceLanguage, TypeArgument, TypeId, TypeIdentity,
    TypeInfo, TypeReference,
};

use super::MAX_RECORD_CHILDREN;
use super::codec::enumeration_constant;
use super::die::strict_flag;
use super::types::{TypeArenaBuilder, TypeEntry};

const DW_AT_GO_KIND: gimli::DwAt = gimli::DwAt(0x2900);
const DW_AT_GO_KEY: gimli::DwAt = gimli::DwAt(0x2901);
const DW_AT_GO_ELEM: gimli::DwAt = gimli::DwAt(0x2902);
const DW_AT_GO_RUNTIME_TYPE: gimli::DwAt = gimli::DwAt(0x2904);

/// The inline namespaces of C++ standard libraries, for units older than
/// DWARF 5's `DW_AT_export_symbols`. libstdc++'s `__cxx1998` is not one: it
/// holds the containers that debug mode's inline `__debug` replaces.
const INLINE_NAMESPACES: [&str; 6] = ["__1", "__Cr", "__ndk1", "__cxx11", "__8", "__debug"];

/// The scopes enclosing a type DIE, outermost first.
#[derive(Clone, Default)]
pub(super) struct ScopePath {
    /// The named scopes.
    pub(super) path: Arc<[Arc<str>]>,
    /// The inline namespaces among them, which names may spell or omit.
    pub(super) inline: Arc<[Arc<str>]>,
}

impl ScopePath {
    pub(super) fn is_empty(&self) -> bool {
        self.path.is_empty() && self.inline.is_empty()
    }
}

/// What a type's identity is built from, recorded as the type is built.
pub(super) struct IdentityParts {
    /// The DIE the type was built from.
    pub(super) die: DieKey,
    /// Arguments from template parameter DIEs, by position.
    pub(super) template: Vec<TypeArgument>,
    /// Where a template parameter pack's arguments begin.
    pub(super) pack: Option<usize>,
    pub(super) go: Option<GoParts>,
}

pub(super) struct GoParts {
    attributes: GoTypeAttributes,
    key: Option<TypeReference>,
    element: Option<TypeReference>,
}

/// How a DIE contributes to the scope path of the types nested in it.
#[derive(Clone)]
pub(super) enum ScopeSegment {
    /// A named type, whose nested types it scopes.
    Named(Arc<str>),
    /// A namespace not marked inline here, which may still be inline: GCC
    /// copies namespaces into type units without their marks. `listed`
    /// when its unit predates the mark and its name is in
    /// [`INLINE_NAMESPACES`].
    Namespace { name: Arc<str>, listed: bool },
    /// A function, whose name may live on its declaration.
    Function(DieKey),
    /// An inline namespace, which names need not spell.
    Inline(Arc<str>),
    /// A scope that names nothing, such as an anonymous type.
    Transparent,
}

/// The offset of a Go type's runtime type descriptor. Go gives the types it
/// synthesizes for its runtime none.
pub(super) fn go_runtime_type(entry: &gimli::DebuggingInformationEntry<Reader<'_>>) -> Option<u64> {
    match entry.attr_value(DW_AT_GO_RUNTIME_TYPE)? {
        gimli::AttributeValue::Addr(offset) => Some(offset),
        value => value.udata_value(),
    }
    .filter(|offset| *offset != 0)
}

pub(super) const fn source_language(language: Option<gimli::DwLang>, zig: bool) -> SourceLanguage {
    if zig {
        // Zig's LLVM backend says its units are C99.
        return SourceLanguage::Zig;
    }
    let Some(language) = language else {
        return SourceLanguage::Unknown;
    };
    match language {
        gimli::DW_LANG_C89
        | gimli::DW_LANG_C
        | gimli::DW_LANG_C99
        | gimli::DW_LANG_C11
        | gimli::DW_LANG_C17 => SourceLanguage::C,
        gimli::DW_LANG_C_plus_plus
        | gimli::DW_LANG_C_plus_plus_03
        | gimli::DW_LANG_C_plus_plus_11
        | gimli::DW_LANG_C_plus_plus_14
        | gimli::DW_LANG_C_plus_plus_17
        | gimli::DW_LANG_C_plus_plus_20 => SourceLanguage::Cpp,
        gimli::DW_LANG_Rust => SourceLanguage::Rust,
        gimli::DW_LANG_Go => SourceLanguage::Go,
        gimli::DW_LANG_Zig => SourceLanguage::Zig,
        other => SourceLanguage::Other(other.0),
    }
}

/// How a DIE scopes the types nested in it, or `None` when it is not a
/// scope. `cpp` when the DIE's unit is C++.
pub(super) fn scope_segment(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
    unit_index: usize,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    cpp: bool,
) -> Option<ScopeSegment> {
    let name = || {
        string_attribute(dwarf, unit, entry, gimli::DW_AT_name)
            .ok()
            .flatten()
    };
    match entry.tag() {
        gimli::DW_TAG_namespace => {
            let exported = entry.attr_value(gimli::DW_AT_export_symbols);
            let inline = strict_flag(entry, gimli::DW_AT_export_symbols).unwrap_or(false);
            Some(match name() {
                Some(name) if inline => ScopeSegment::Inline(name),
                None if inline => ScopeSegment::Transparent,
                Some(name) => ScopeSegment::Namespace {
                    // Producers older than the mark still use these names
                    // for inline namespaces.
                    listed: cpp
                        && unit.encoding().version < 5
                        && exported.is_none()
                        && INLINE_NAMESPACES.contains(&name.as_ref()),
                    name,
                },
                None => ScopeSegment::Namespace {
                    name: Arc::from(ANONYMOUS_NAMESPACE),
                    listed: false,
                },
            })
        }
        gimli::DW_TAG_structure_type
        | gimli::DW_TAG_class_type
        | gimli::DW_TAG_union_type
        | gimli::DW_TAG_enumeration_type => {
            Some(name().map_or(ScopeSegment::Transparent, ScopeSegment::Named))
        }
        gimli::DW_TAG_subprogram => Some(ScopeSegment::Function(DieKey {
            unit: unit_index,
            offset: entry.offset().0,
        })),
        _ => None,
    }
}

/// The full path of an inline namespace, its own name last, from the
/// segments enclosing it.
pub(super) fn inline_namespace_path(
    segments: &[(isize, ScopeSegment)],
    name: &Arc<str>,
) -> Vec<Arc<str>> {
    segments
        .iter()
        .filter_map(|(_, segment)| match segment {
            ScopeSegment::Named(name)
            | ScopeSegment::Namespace { name, .. }
            | ScopeSegment::Inline(name) => Some(Arc::clone(name)),
            ScopeSegment::Function(_) | ScopeSegment::Transparent => None,
        })
        .chain(std::iter::once(Arc::clone(name)))
        .collect()
}

impl<'data> TypeArenaBuilder<'_, 'data> {
    /// A function's name, from its own DIE or the declaration it completes.
    pub(super) fn function_name(&self, key: DieKey) -> Option<Arc<str>> {
        let mut current = key;
        for _ in 0..4 {
            let unit = self.units.get(current.unit)?;
            let entry = unit.entry(gimli::UnitOffset(current.offset)).ok()?;
            if entry.attr_value(gimli::DW_AT_name).is_some() {
                return string_attribute(self.dwarf, unit, &entry, gimli::DW_AT_name).ok()?;
            }
            let reference = entry
                .attr_value(gimli::DW_AT_specification)
                .or_else(|| entry.attr_value(gimli::DW_AT_abstract_origin));
            current = die_reference_with_signatures(
                reference,
                current.unit,
                self.units,
                self.type_signatures,
            )
            .ok()??;
        }
        None
    }

    /// Resolves a scope stack into a path. A namespace is inline when any
    /// unit marks the namespace with the same full path inline, or when no
    /// unit marks any namespace and its own unit lists it.
    pub(super) fn scope_path(
        &self,
        segments: &[ScopeSegment],
        inline_namespaces: &HashSet<Vec<Arc<str>>>,
    ) -> ScopePath {
        let mut path = Vec::new();
        let mut inline = Vec::new();
        let mut full = Vec::new();
        for segment in segments {
            match segment {
                ScopeSegment::Named(name) => {
                    full.push(Arc::clone(name));
                    path.push(Arc::clone(name));
                }
                ScopeSegment::Namespace { name, listed } => {
                    full.push(Arc::clone(name));
                    if inline_namespaces.contains(&full) || *listed && inline_namespaces.is_empty()
                    {
                        inline.push(Arc::clone(name));
                    } else {
                        path.push(Arc::clone(name));
                    }
                }
                ScopeSegment::Function(key) => {
                    let name = self.function_name(*key);
                    full.extend(name.clone());
                    path.extend(name);
                }
                ScopeSegment::Inline(name) => {
                    full.push(Arc::clone(name));
                    inline.push(Arc::clone(name));
                }
                ScopeSegment::Transparent => {}
            }
        }
        ScopePath {
            path: path.into(),
            inline: inline.into(),
        }
    }

    pub(super) fn language(&self, unit_index: usize) -> SourceLanguage {
        source_language(
            self.unit_languages.get(unit_index).copied().flatten(),
            self.zig_units.get(unit_index).copied().unwrap_or(false),
        )
    }

    /// Records what a named type's identity is built from.
    pub(super) fn record_identity_parts(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        die: DieKey,
        id: TypeId,
    ) {
        let (template, pack) = match entry.tag() {
            gimli::DW_TAG_structure_type
            | gimli::DW_TAG_class_type
            | gimli::DW_TAG_union_type
            | gimli::DW_TAG_enumeration_type
            | gimli::DW_TAG_template_alias => self.template_arguments(entry, die.unit),
            _ => (Vec::new(), None),
        };
        let go = if self.language(die.unit) == SourceLanguage::Go {
            self.go_parts(entry, die.unit)
        } else {
            None
        };
        self.identity_parts.insert(
            id,
            IdentityParts {
                die,
                template,
                pack,
                go,
            },
        );
    }

    /// The arguments a type's template parameter DIEs describe, with packs
    /// flattened, and where its pack begins. A parameter whose value or type
    /// cannot be read is unknown.
    fn template_arguments(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> (Vec<TypeArgument>, Option<usize>) {
        let mut arguments = Vec::new();
        let mut pack = None;
        for child in self.child_entries(entry, unit_index) {
            if child.tag() == gimli::DW_TAG_GNU_template_parameter_pack {
                pack = pack.or(Some(arguments.len()));
                for parameter in self.child_entries(&child, unit_index) {
                    if let Some(argument) = self.parameter_argument(&parameter, unit_index) {
                        arguments.push(argument);
                    }
                }
            } else if let Some(argument) = self.parameter_argument(&child, unit_index) {
                arguments.push(argument);
            }
        }
        (arguments, pack)
    }

    /// A DIE's children, bounded as a record's are.
    fn child_entries(
        &self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> Vec<gimli::DebuggingInformationEntry<Reader<'data>>> {
        let mut found = Vec::new();
        let Some(unit) = self.units.get(unit_index) else {
            return found;
        };
        let Ok(mut tree) = unit.entries_tree(Some(entry.offset())) else {
            return found;
        };
        let Ok(root) = tree.root() else {
            return found;
        };
        let mut children = root.children();
        while let Ok(Some(child)) = children.next() {
            if found.len() >= MAX_RECORD_CHILDREN {
                break;
            }
            found.push(child.entry().clone());
        }
        found
    }

    /// The argument one template parameter DIE describes.
    fn parameter_argument(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> Option<TypeArgument> {
        Some(match entry.tag() {
            gimli::DW_TAG_template_type_parameter => match self.target(entry, unit_index) {
                Ok(Some(target)) => TypeArgument::Type(target),
                // A type parameter without a type is `void`.
                Ok(None) => TypeArgument::Type(self.void_type()),
                Err(_) => self.unknown_argument(entry, unit_index),
            },
            gimli::DW_TAG_template_value_parameter => self.template_value(entry, unit_index),
            gimli::DW_TAG_GNU_template_template_param => {
                let name = self.units.get(unit_index).and_then(|unit| {
                    string_attribute(self.dwarf, unit, entry, gimli::DW_AT_GNU_template_name)
                        .ok()
                        .flatten()
                });
                name.map_or_else(
                    || self.unknown_argument(entry, unit_index),
                    TypeArgument::Unknown,
                )
            }
            _ => return None,
        })
    }

    /// A value parameter's integer, decoded by its type.
    fn template_value(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> TypeArgument {
        let value = entry
            .attr_value(gimli::DW_AT_const_value)
            .and_then(|value| {
                let target = self.target(entry, unit_index).ok()??;
                let base = self.resolved_integer_base(target.id).ok()?;
                enumeration_constant(value, &base, self.byte_order).ok()
            });
        value.map_or_else(
            || self.unknown_argument(entry, unit_index),
            TypeArgument::Value,
        )
    }

    /// An argument known only by its parameter's name.
    fn unknown_argument(
        &self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> TypeArgument {
        let name = self.units.get(unit_index).and_then(|unit| {
            string_attribute(self.dwarf, unit, entry, gimli::DW_AT_name)
                .ok()
                .flatten()
        });
        TypeArgument::Unknown(name.unwrap_or_else(|| Arc::from("?")))
    }

    /// Go's kind, runtime type, key, and element for a type DIE.
    fn go_parts(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> Option<GoParts> {
        let kind = Self::go_kind(entry)?;
        let mut reference = |attribute| {
            let key = die_reference_with_signatures(
                entry.attr_value(attribute),
                unit_index,
                self.units,
                self.type_signatures,
            )
            .ok()??;
            Some(TypeReference {
                image: self.image,
                id: self.resolve(key),
            })
        };
        let (key, element) = match kind {
            GoKind::Map => (reference(DW_AT_GO_KEY), reference(DW_AT_GO_ELEM)),
            GoKind::Chan | GoKind::Slice | GoKind::Array | GoKind::Pointer => {
                (None, reference(DW_AT_GO_ELEM))
            }
            _ => (None, None),
        };
        Some(GoParts {
            attributes: GoTypeAttributes {
                kind,
                runtime_type: go_runtime_type(entry),
            },
            key,
            element,
        })
    }

    /// The Go kind a type DIE records, if any.
    pub(super) fn go_kind(
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
    ) -> Option<GoKind> {
        let kind = match entry.attr_value(DW_AT_GO_KIND)? {
            gimli::AttributeValue::Data1(kind) => kind,
            value => u8::try_from(value.udata_value()?).ok()?,
        };
        Some(GoKind::from_abi(kind))
    }

    /// Gives every named type its identity, once names are final.
    pub(super) fn assign_identities(&mut self) {
        let mut unresolved = Vec::new();
        for index in 0..self.entries.len() {
            let id = TypeId::new(u32::try_from(index).expect("type count fits u32"));
            if !self.explicit_names.contains(&id) {
                continue;
            }
            let Some(TypeEntry::Resolved(info)) = self.entries.get(index) else {
                continue;
            };
            let Some(parts) = self.identity_parts.get(&id) else {
                continue;
            };
            let language = self.language(parts.die.unit);
            let syntax = NameSyntax::of(language);
            let name = Arc::clone(&info.name);
            let parsed = TypeName::parse(&name, syntax);
            let scopes = self.type_path(parts.die);
            // Only Go and Zig names spell their packages and modules.
            let mut path = scopes.path.to_vec();
            path.extend(parsed.path.iter().map(|segment| Arc::<str>::from(*segment)));
            let (arguments, origin, pending) = merge_arguments(parts, parsed.arguments.as_deref());
            if !pending.is_empty() {
                unresolved.push((index, language, pending));
            }
            let pack = parts.pack.filter(|start| *start <= arguments.len());
            let identity = TypeIdentity {
                language,
                path: path.into(),
                inline_namespaces: scopes.inline,
                base: Arc::from(parsed.base),
                arguments: arguments.into(),
                pack,
                origin,
                go: parts.go.as_ref().map(|go| go.attributes),
            };
            if let Some(TypeEntry::Resolved(info)) = self.entries.get_mut(index) {
                info.identity = Some(Arc::new(identity));
            }
        }
        self.resolve_parsed_arguments(&unresolved);
    }

    /// The scopes enclosing the DIE a type was built from, or those of the
    /// declaration it completes.
    fn type_path(&self, die: DieKey) -> ScopePath {
        self.type_scopes
            .get(&die)
            .or_else(|| {
                self.definition_declarations
                    .get(&die)
                    .and_then(|declaration| self.type_scopes.get(declaration))
            })
            .cloned()
            .unwrap_or_default()
    }

    /// Resolves the arguments parsed from names, now that every identity
    /// exists. An argument resolves when the types it could name are one.
    fn resolve_parsed_arguments(&mut self, unresolved: &[(usize, SourceLanguage, Vec<usize>)]) {
        if unresolved.is_empty() {
            return;
        }
        let lookup = EntryLookup(&self.entries);
        let index = TypeIndex::build(Some(self.image), self.entries.len(), |index| {
            match self.entries.get(index) {
                Some(TypeEntry::Resolved(info)) => Some(info),
                _ => None,
            }
        });
        // The first pointer type to each type, by the target's identity, for
        // arguments spelled as pointers.
        let mut pointers = HashMap::new();
        let spelled_pointer = unresolved.iter().any(|(entry, _, positions)| {
            matches!(
                self.entries.get(*entry),
                Some(TypeEntry::Resolved(TypeInfo { identity: Some(identity), .. }))
                    if positions.iter().any(|position| matches!(
                        identity.arguments.get(*position),
                        Some(TypeArgument::Unknown(text)) if text.trim_end().ends_with('*')
                    ))
            )
        });
        if spelled_pointer {
            for entry in &self.entries {
                if let TypeEntry::Resolved(TypeInfo {
                    reference,
                    kind:
                        crate::TypeKind::Pointer {
                            target: Some(target),
                            ..
                        },
                    ..
                }) = entry
                    && let Some(key) = index.key(*target)
                {
                    pointers.entry(Arc::clone(key)).or_insert(*reference);
                }
            }
        }
        let mut resolved = Vec::new();
        for (entry, language, positions) in unresolved {
            let Some(TypeEntry::Resolved(info)) = self.entries.get(*entry) else {
                continue;
            };
            let Some(identity) = &info.identity else {
                continue;
            };
            let mut arguments = identity.arguments.to_vec();
            for position in positions {
                let TypeArgument::Unknown(text) = &arguments[*position] else {
                    continue;
                };
                if let Some(found) = resolve_argument(text, *language, &index, &lookup, &pointers) {
                    arguments[*position] = found;
                }
            }
            resolved.push((*entry, arguments));
        }
        for (entry, arguments) in resolved {
            if let Some(TypeEntry::Resolved(info)) = self.entries.get_mut(entry)
                && let Some(identity) = &mut info.identity
            {
                Arc::make_mut(identity).arguments = arguments.into();
            }
        }
    }
}

struct EntryLookup<'a>(&'a [TypeEntry]);

impl TypeLookup for EntryLookup<'_> {
    fn type_info(&self, reference: TypeReference) -> Option<&TypeInfo> {
        match self.0.get(reference.id.index())? {
            TypeEntry::Resolved(info) => Some(info),
            _ => None,
        }
    }
}

/// Combines the arguments DWARF describes with those a name spells. Returns
/// the arguments, their origin, and the positions still to resolve.
fn merge_arguments(
    parts: &IdentityParts,
    named: Option<&[&str]>,
) -> (Vec<TypeArgument>, ArgumentOrigin, Vec<usize>) {
    if let Some(go) = &parts.go {
        let described = match (go.key, go.element) {
            (Some(key), Some(element)) => vec![key, element],
            (None, Some(element)) => vec![element],
            _ => Vec::new(),
        };
        if !described.is_empty() {
            return (
                described.into_iter().map(TypeArgument::Type).collect(),
                ArgumentOrigin::Dwarf,
                Vec::new(),
            );
        }
    }
    let unknown = |text: &str| TypeArgument::Unknown(Arc::from(text));
    let parsed = |texts: &[&str]| {
        let arguments = texts.iter().map(|text| unknown(text)).collect::<Vec<_>>();
        let pending = (0..arguments.len()).collect();
        (arguments, ArgumentOrigin::ParsedName, pending)
    };
    let template = &parts.template;
    match named {
        _ if template.is_empty() => match named {
            Some(texts) if !texts.is_empty() => parsed(texts),
            _ => (Vec::new(), ArgumentOrigin::None, Vec::new()),
        },
        // The name spells each parameter: an unreadable one takes its
        // spelling, which may still resolve.
        Some(texts) if texts.len() == template.len() => {
            let mut pending = Vec::new();
            let arguments = template
                .iter()
                .zip(texts)
                .enumerate()
                .map(|(position, (argument, text))| match argument {
                    TypeArgument::Unknown(_) => {
                        pending.push(position);
                        unknown(text)
                    }
                    known => known.clone(),
                })
                .collect();
            (arguments, ArgumentOrigin::Dwarf, pending)
        }
        // rustc omits const parameters: the name's integers fill the
        // positions the parameters skip, if that accounts for every one.
        Some(texts) if texts.len() > template.len() => {
            let mut described = template.iter();
            let merged = texts
                .iter()
                .map(|text| {
                    parse_integer(text)
                        .map(TypeArgument::Value)
                        .or_else(|| described.next().cloned())
                })
                .collect::<Option<Vec<_>>>();
            match merged {
                Some(arguments) if described.next().is_none() => {
                    (arguments, ArgumentOrigin::Dwarf, Vec::new())
                }
                // Positions the parameters cannot be placed in come from
                // the name alone, rather than out of place.
                _ => parsed(texts),
            }
        }
        _ => (template.clone(), ArgumentOrigin::Dwarf, Vec::new()),
    }
}

/// What a parsed argument spells: a value, or the one type it names in the
/// type's own language. Types several units define alike are one type;
/// several distinct types, or none, leave it unknown.
fn resolve_argument(
    text: &str,
    language: SourceLanguage,
    index: &TypeIndex,
    lookup: &EntryLookup<'_>,
    pointers: &HashMap<Arc<str>, TypeReference>,
) -> Option<TypeArgument> {
    if let Some(value) = parse_integer(text) {
        return Some(TypeArgument::Value(value));
    }
    // A pointer, as GCC spells `int*` in a pack it leaves out: the pointer
    // type whose target is the type its spelling resolves to.
    if let Some(target) = text.trim_end().strip_suffix('*') {
        let Some(TypeArgument::Type(target)) =
            resolve_argument(target.trim_end(), language, index, lookup, pointers)
        else {
            return None;
        };
        return pointers
            .get(index.key(target)?)
            .copied()
            .map(TypeArgument::Type);
    }
    let candidates = index
        .named(text, true, lookup)
        .into_iter()
        .filter_map(|reference| lookup.type_info(reference))
        .filter(|info| {
            info.identity
                .as_deref()
                .is_none_or(|identity| identity.language == language)
        })
        .collect::<Vec<_>>();
    let first = candidates.first()?;
    candidates
        .iter()
        .all(|other| index.same_type(first.reference, other.reference))
        .then_some(TypeArgument::Type(first.reference))
}
