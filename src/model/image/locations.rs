//! The functions and lines a breakpoint location names in one image.
//!
//! A function location is first a function's whole name, as the debug
//! information spells it. Failing that, a function a package defines is
//! named as its language's programmers write it: by its local name within
//! the package (see [`crate::type_identity::functions`]), qualified by the
//! package's import path or name, or not qualified at all. A qualified
//! match wins over an unqualified one. Wrappers are never named, and a
//! location naming functions of more than one package or local name is
//! ambiguous rather than bound to all of them.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::type_identity::{NameSyntax, functions};
use crate::{Error, Result};

use super::{
    CodeRole, FunctionId, FunctionInfo, LineNumber, ModuleImage, SourceFileId, SourceLanguage,
};

/// A unit of code that a language names by an import path and, in its
/// own source, by a shorter name, such as a Go package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageInfo {
    /// The path other code imports the package by, such as `net/http`.
    pub path: Arc<str>,
    /// The name the package's code declares, such as `http`.
    pub name: Arc<str>,
}

/// A function as its package names it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PackagedName {
    package: Arc<str>,
    local: Arc<str>,
}

/// The image's functions by their names within their packages.
#[derive(Debug)]
pub(super) struct FunctionNames {
    /// Each package's name, by its path.
    packages: BTreeMap<Arc<str>, Arc<str>>,
    /// Each function's package and local name, by function index.
    names: Box<[Option<PackagedName>]>,
    by_local: BTreeMap<Arc<str>, Arc<[FunctionId]>>,
}

impl FunctionNames {
    pub(super) fn new(functions: &[FunctionInfo], packages: &[PackageInfo]) -> Self {
        let packages = packages
            .iter()
            .map(|package| (Arc::clone(&package.path), Arc::clone(&package.name)))
            .collect::<BTreeMap<_, _>>();
        let names = functions
            .iter()
            .map(|function| {
                let (package, local) = functions::packaged_name(
                    &function.name,
                    NameSyntax::of(function.language),
                    |path| packages.contains_key(path),
                )?;
                Some(PackagedName {
                    package: package.into(),
                    local: local.into(),
                })
            })
            .collect::<Box<[_]>>();
        let by_local =
            super::grouped_index(functions.iter().zip(&names).filter_map(|(function, name)| {
                Some((Arc::clone(&name.as_ref()?.local), function.id))
            }));
        Self {
            packages,
            names,
            by_local,
        }
    }

    fn name(&self, function: FunctionId) -> Option<&PackagedName> {
        self.names.get(function.index())?.as_ref()
    }

    /// The functions whose local name, qualified by their package's path
    /// or name, is `plain`.
    fn qualified(&self, plain: &str) -> Vec<FunctionId> {
        let mut found = Vec::new();
        for (dot, _) in plain.match_indices('.') {
            let (qualifier, local) = (&plain[..dot], &plain[dot + 1..]);
            for &function in self.unqualified(local) {
                let package = &self.name(function).expect("indexed by name").package;
                if &**package == qualifier
                    || self
                        .packages
                        .get(package)
                        .is_some_and(|name| &**name == qualifier)
                {
                    found.push(function);
                }
            }
        }
        found
    }

    fn unqualified(&self, plain: &str) -> &[FunctionId] {
        self.by_local.get(plain).map_or(&[], |functions| functions)
    }
}

/// What makes two matched functions the same one: a package and local
/// name, which instantiations and inlined copies share, or else the
/// function itself.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Identity<'a> {
    Packaged(&'a PackagedName),
    Function(FunctionId),
}

impl ModuleImage {
    /// Finds the functions with code that a breakpoint location names,
    /// keeping only those declared in `file` when it is given.
    ///
    /// The location is first a whole name, which every function so named
    /// matches, as overloads do in gdb. Failing that, a function a package
    /// defines matches its local name within the package, such as
    /// `Stack.Push` for `main.(*Stack[go.shape.int]).Push`, qualified by the
    /// package's import path or name, or else unqualified; receivers may
    /// be written with or without `(*…)`. Wrappers never match. A function
    /// that a package defines counts as declared in a file when any copy of
    /// it is, since copies inlined into other units may not say where they
    /// come from.
    ///
    /// Fails with [`Error::AmbiguousFunction`], naming each candidate by
    /// its package and local name, when the matches are more than one such
    /// function, and with [`Error::FunctionNotFound`] when nothing matches.
    pub fn functions_located(
        &self,
        location: &str,
        file: Option<SourceFileId>,
    ) -> Result<Vec<&FunctionInfo>> {
        let mut found = self
            .functions_named(location)
            .filter(|function| self.locatable(function))
            .collect::<Vec<_>>();
        if found.is_empty() {
            found = self.packaged_functions(location);
        }
        let identity = |function: &FunctionInfo| {
            self.function_names
                .name(function.id)
                .map_or(Identity::Function(function.id), Identity::Packaged)
        };
        if let Some(file) = file {
            let declared = found
                .iter()
                .filter(|function| {
                    function
                        .declaration
                        .as_ref()
                        .is_some_and(|declaration| declaration.file == file)
                })
                .map(|function| identity(function))
                .collect::<BTreeSet<_>>();
            found.retain(|function| declared.contains(&identity(function)));
        }
        let packaged = found
            .iter()
            .filter_map(|function| self.function_names.name(function.id))
            .collect::<BTreeSet<_>>();
        if packaged.len() > 1 {
            return Err(Error::AmbiguousFunction {
                name: location.to_owned(),
                candidates: packaged
                    .into_iter()
                    .map(|name| format!("{}.{}", name.package, name.local))
                    .collect(),
            });
        }
        if found.is_empty() {
            return Err(Error::FunctionNotFound(location.to_owned()));
        }
        Ok(found)
    }

    /// The import path of the package that defines a function, for
    /// languages with packages.
    #[must_use]
    pub fn function_package(&self, function: FunctionId) -> Option<&Arc<str>> {
        Some(&self.function_names.name(function)?.package)
    }

    /// The location an unqualified `location` means within `package`'s
    /// scope: qualified by the package, when nothing in the image has the
    /// whole name or a qualified name it spells and the package has a
    /// function of that local name. Code stopped in a package names its
    /// own functions unqualified, as its source does.
    #[must_use]
    pub fn location_in_package(&self, location: &str, package: &str) -> Option<String> {
        if self
            .functions_named(location)
            .any(|function| self.locatable(function))
        {
            return None;
        }
        let plain = functions::plain_location(location)?;
        let names = &self.function_names;
        let locatable = |function: &FunctionId| {
            self.function(*function)
                .is_some_and(|function| self.locatable(function))
        };
        if names.qualified(&plain).iter().any(locatable) {
            return None;
        }
        names
            .unqualified(&plain)
            .iter()
            .filter(|function| locatable(function))
            .any(|&function| {
                names
                    .name(function)
                    .is_some_and(|name| &*name.package == package)
            })
            .then(|| format!("{package}.{location}"))
    }

    /// The functions that a package defines and that `location` names by
    /// their local names: qualified ones if any, otherwise unqualified.
    fn packaged_functions(&self, location: &str) -> Vec<&FunctionInfo> {
        let Some(plain) = functions::plain_location(location) else {
            return Vec::new();
        };
        let functions = |ids: &[FunctionId]| {
            ids.iter()
                .filter_map(|function| self.function(*function))
                .filter(|function| self.locatable(function))
                .collect::<Vec<_>>()
        };
        let qualified = functions(&self.function_names.qualified(&plain));
        if qualified.is_empty() {
            functions(self.function_names.unqualified(&plain))
        } else {
            qualified
        }
    }

    /// Whether a location can name a function: it has code, and is not a
    /// wrapper, which only forwards to the function a location means.
    fn locatable(&self, function: &FunctionInfo) -> bool {
        function.role != CodeRole::Wrapper
            && self.instances_for_function(function.id).next().is_some()
    }

    /// Whether a line breakpoint in a file stays at the line it asks for:
    /// the file's functions are Go, whose compiler marks a statement on
    /// every line that has code, so a line without one has no code to
    /// stop at and moving to another line would stop somewhere else.
    #[must_use]
    pub fn keeps_line_breakpoints(&self, file: SourceFileId) -> bool {
        self.functions.iter().any(|function| {
            function.language == SourceLanguage::Go
                && function
                    .declaration
                    .as_ref()
                    .is_some_and(|declaration| declaration.file == file)
        })
    }

    /// The nearest lines of a file before and after `line` that have
    /// statements.
    #[must_use]
    pub fn nearest_statement_lines(
        &self,
        file: SourceFileId,
        line: LineNumber,
    ) -> (Option<LineNumber>, Option<LineNumber>) {
        let before = self
            .statements_by_source_line
            .range(..(file, line))
            .next_back()
            .filter(|((other, _), _)| *other == file)
            .map(|((_, line), _)| *line);
        let after = line
            .get()
            .checked_add(1)
            .and_then(LineNumber::new)
            .and_then(|next| {
                self.statements_by_source_line
                    .range((file, next)..)
                    .next()
                    .filter(|((other, _), _)| *other == file)
                    .map(|((_, line), _)| *line)
            });
        (before, after)
    }
}
