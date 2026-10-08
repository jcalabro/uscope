//! The catalog of global data objects, deduplicated across units.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::debug_info::dwarf::{DieKey, DwarfError, Reader, is_type_unit};
use crate::{
    GlobalVariableId, GlobalVariableInfo, GlobalVariableType, GlobalVariableVisibility, SourceFile,
    SourceFileId, VariableKind, VariableMalformedKind,
};

use super::die::{
    check_data_object_capacity, copy_name, debug_info_offset, declaration_with_origins,
    flag_with_origins, origin_chain, string_with_origins, type_with_origins,
};
use super::location::copy_data_object_value_with_origins;
use super::types::{TypeArenaBuilder, TypeEntry, TypeResolution};
use super::{CatalogDataObject, Metadata, MetadataAbsence, ValueDescription, malformed_reason};

#[derive(Clone, Default)]
pub(super) struct GlobalScope {
    pub(super) path: Arc<[Arc<str>]>,
    pub(super) routine: bool,
}

/// The scope of every DIE outside type units: the distinct scopes, the
/// first the empty one, and for each unit its DIEs' offsets in order with
/// the scope each has. Most DIEs share their parent's.
struct ScopeTable {
    scopes: Vec<GlobalScope>,
    units: Vec<UnitScopes>,
}

#[derive(Default)]
struct UnitScopes {
    offsets: Vec<usize>,
    scopes: Vec<u32>,
}

impl ScopeTable {
    fn get(&self, key: DieKey) -> Option<&GlobalScope> {
        let unit = self.units.get(key.unit)?;
        let index = unit.offsets.binary_search(&key.offset).ok()?;
        self.scopes.get(usize::try_from(unit.scopes[index]).ok()?)
    }

    /// A scope's index once it is in the table.
    fn add(&mut self, scope: GlobalScope) -> u32 {
        self.scopes.push(scope);
        u32::try_from(self.scopes.len() - 1).expect("scope count fits u32")
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum DefinitionResolution {
    New,
    Existing(usize),
    Conflict,
}

#[derive(Default)]
pub(super) struct DefinitionIndex {
    pub(super) by_identity: HashMap<Arc<str>, usize>,
}

impl DefinitionIndex {
    pub(super) fn resolve(
        &mut self,
        identities: &[Arc<str>],
        candidate: usize,
    ) -> DefinitionResolution {
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
pub(super) fn load_globals<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    units: &[gimli::Unit<Reader<'data>>],
    objects: &mut Vec<CatalogDataObject>,
    order: &mut u64,
    source_files: &mut Vec<SourceFile>,
    source_file_ids: &mut HashMap<PathBuf, SourceFileId>,
    types: &mut TypeArenaBuilder<'_, 'data>,
) -> std::result::Result<(Vec<GlobalVariableInfo>, Vec<usize>), DwarfError> {
    let mut table = ScopeTable {
        scopes: vec![GlobalScope::default()],
        units: Vec::with_capacity(units.len()),
    };

    // Pass one records lexical ownership for every DIE. A later definition
    // may point backward to a declaration nested in a namespace or class.
    for unit in units {
        let mut unit_scopes = UnitScopes::default();
        if !is_type_unit(unit) {
            let mut entries = unit.entries();
            let mut scopes = Vec::<u32>::new();
            while let Some(entry) = entries.next_dfs()? {
                let depth =
                    usize::try_from(entry.depth()).map_err(|_| DwarfError::InvalidEntryDepth)?;
                scopes.truncate(depth);
                let parent = scopes.last().copied().unwrap_or(0);
                let parent_scope = &table.scopes[usize::try_from(parent).expect("u32 fits usize")];
                let scope = match entry.tag() {
                    gimli::DW_TAG_subprogram | gimli::DW_TAG_inlined_subroutine
                        if !parent_scope.routine =>
                    {
                        let path = Arc::clone(&parent_scope.path);
                        table.add(GlobalScope {
                            path,
                            routine: true,
                        })
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
                        let mut path = parent_scope.path.to_vec();
                        path.push(component);
                        let routine = parent_scope.routine;
                        table.add(GlobalScope {
                            path: path.into(),
                            routine,
                        })
                    }
                    _ => parent,
                };
                unit_scopes.offsets.push(entry.offset().0);
                unit_scopes.scopes.push(scope);
                scopes.push(scope);
            }
        }
        // Entries follow one another, so their offsets are already in order.
        if !unit_scopes.offsets.is_sorted() {
            let mut pairs = unit_scopes
                .offsets
                .iter()
                .copied()
                .zip(unit_scopes.scopes.iter().copied())
                .collect::<Vec<_>>();
            pairs.sort_by_key(|(offset, _)| *offset);
            (unit_scopes.offsets, unit_scopes.scopes) = pairs.into_iter().unzip();
        }
        table.units.push(unit_scopes);
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
            let current_scope = table.get(key).unwrap_or(&table.scopes[0]);
            if current_scope.routine {
                continue;
            }

            let (chain, chain_error) = match origin_chain(units, unit_index, entry) {
                Ok(chain) => (chain, None),
                Err(error) => (Vec::new(), Some(Arc::from(error.to_string()))),
            };
            let name =
                match string_with_origins(dwarf, units, unit, entry, &chain, gimli::DW_AT_name) {
                    Ok(Some(name)) => name,
                    Ok(None) => format!("<anonymous global at {:#x}>", entry.offset().0).into(),
                    Err(error) => {
                        format!("<malformed global at {:#x}: {error}>", entry.offset().0).into()
                    }
                };
            // A malformed linkage name marks this entry malformed, not the module.
            let linkage =
                |attribute| string_with_origins(dwarf, units, unit, entry, &chain, attribute);
            let linkage_result = match linkage(gimli::DW_AT_linkage_name) {
                Ok(None) => linkage(gimli::DW_AT_MIPS_linkage_name),
                result => result,
            };
            let (linkage_name, linkage_error) = match linkage_result {
                Ok(name) => (name, None),
                Err(error) => (None, Some(Arc::<str>::from(error.to_string()))),
            };
            let scope = chain
                .iter()
                .filter_map(|(origin_unit, origin)| {
                    table.get(DieKey {
                        unit: *origin_unit,
                        offset: origin.offset().0,
                    })
                })
                .chain(std::iter::once(current_scope))
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
            let (type_unit, type_value) = type_with_origins(unit_index, entry, &chain);
            let type_info = types.variable_type(type_unit, type_value);
            let value =
                copy_data_object_value_with_origins(dwarf, units, unit_index, unit, entry, &chain);
            let declaration_only = flag_with_origins(entry, &chain, gimli::DW_AT_declaration)
                .unwrap_or(false)
                && matches!(value, Metadata::Absent(_));
            if declaration_only {
                continue;
            }
            let visibility =
                if flag_with_origins(entry, &chain, gimli::DW_AT_external).unwrap_or(false) {
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
                debug_info_offset: debug_info_offset(unit, entry),
                kind: VariableKind::Global,
                name: Arc::clone(&name),
                declaration: declaration.as_ref().ok().cloned().flatten(),
                ranges: Vec::new().into(),
                instance: None,
                lexical_depth: 0,
                order: *order,
                type_info: type_info.clone(),
                escaped: None,
                hidden: false,
                coroutine: None,
                value,
                frame_base: Metadata::Absent(MetadataAbsence::NotApplicable),
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
            check_data_object_capacity(objects.len())?;
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
        Metadata::Absent(_) => 1,
        Metadata::Malformed(_) => 0,
    }
}

pub(super) fn public_global_type(
    resolution: &TypeResolution,
    types: &[TypeEntry],
) -> GlobalVariableType {
    match resolution {
        TypeResolution::Resolved(id) => match types.get(id.index()) {
            Some(TypeEntry::Resolved(value)) => GlobalVariableType::Resolved(value.clone()),
            Some(TypeEntry::Malformed(description)) => {
                GlobalVariableType::Malformed(malformed_reason(
                    VariableMalformedKind::InvalidTypeGraph,
                    Arc::clone(description),
                ))
            }
            Some(TypeEntry::Building) | None => GlobalVariableType::Malformed(malformed_reason(
                VariableMalformedKind::InvalidTypeGraph,
                "type graph did not finish building".into(),
            )),
        },
        TypeResolution::Malformed(description) => GlobalVariableType::Malformed(malformed_reason(
            VariableMalformedKind::InvalidTypeGraph,
            Arc::clone(description),
        )),
    }
}
