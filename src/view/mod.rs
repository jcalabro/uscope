//! Views: how a value of a type a view matches is presented as the thing
//! it stands for, such as a `std::string` as its text or a `Vec` as its
//! elements, while the stored value stays one step away
//! (`docs/views.md`).
//!
//! This module is pure, as the evaluator is: it reaches a program only
//! through the evaluator's traits, which the debugger implements, and never
//! performs I/O, reads clocks, or starts threads. A view is parsed once,
//! bound against each concrete type it matches before anything runs, and
//! then run at stops, charging every read to the inspection's budget.

pub mod bind;
#[cfg(any(test, feature = "fuzzing"))]
pub mod fuzz;
pub mod pattern;
pub mod run;
pub mod summary;
pub mod syntax;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use crate::eval::target::Scope;
use crate::eval::types::representation;
use crate::{TypeReference, ViewName};

use bind::{BoundView, Rejection};
use syntax::View;

/// The view files built into uscope, in the order they are tried.
const BUILT_IN: [(&str, &str); 4] = [
    (
        "libstdc++.views",
        include_str!("../../views/libstdc++.views"),
    ),
    ("libc++.views", include_str!("../../views/libc++.views")),
    ("rust-std.views", include_str!("../../views/rust-std.views")),
    ("zig-std.views", include_str!("../../views/zig-std.views")),
];

/// Every view uscope knows, in the order they are tried: those loaded for
/// the session, then the built-in ones. Immutable once made; loading views
/// makes a new set.
#[derive(Debug)]
pub struct ViewSet {
    views: Vec<Arc<View>>,
    /// Each base name's views, in order.
    by_base: BTreeMap<String, Vec<usize>>,
    errors: Vec<syntax::Error>,
}

impl ViewSet {
    /// Parses files, highest precedence first.
    #[must_use]
    pub fn new<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut views = Vec::new();
        let mut errors = Vec::new();
        for (source, text) in files {
            let file = syntax::parse(source, text);
            views.extend(file.views);
            errors.extend(file.errors);
        }
        let mut by_base = BTreeMap::<String, Vec<usize>>::new();
        for (index, view) in views.iter().enumerate() {
            by_base
                .entry(view.pattern.base.clone())
                .or_default()
                .push(index);
        }
        Self {
            views,
            by_base,
            errors,
        }
    }

    /// The built-in views alone.
    #[must_use]
    pub fn built_in() -> Arc<Self> {
        static BUILT_IN_SET: OnceLock<Arc<ViewSet>> = OnceLock::new();
        Arc::clone(BUILT_IN_SET.get_or_init(|| Arc::new(Self::new(BUILT_IN))))
    }

    /// Session files ahead of the built-in views.
    #[must_use]
    pub fn with_session<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        Self::new(files.into_iter().chain(BUILT_IN))
    }

    /// The views, in the order they are tried.
    #[must_use]
    pub fn views(&self) -> &[Arc<View>] {
        &self.views
    }

    /// What kept parts of the files out.
    #[must_use]
    pub fn errors(&self) -> &[syntax::Error] {
        &self.errors
    }
}

/// A view a type was matched against, and whether it bound.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub name: Arc<ViewName>,
    /// `None` when it bound.
    pub rejection: Option<Rejection>,
}

/// Which view presents a type: the first candidate that binds, with why
/// each candidate before it did not. Candidates after it are not tried.
#[derive(Debug, Clone)]
pub struct Choice<St> {
    pub bound: Option<Arc<BoundView<St>>>,
    pub candidates: Vec<Candidate>,
}

impl<St> Default for Choice<St> {
    fn default() -> Self {
        Self {
            bound: None,
            candidates: Vec::new(),
        }
    }
}

/// How a view is named in results and diagnostics.
#[must_use]
pub fn name_of(view: &View) -> Arc<ViewName> {
    Arc::new(ViewName {
        source: Arc::clone(&view.source),
        line: view.line,
        header: Arc::clone(&view.header),
    })
}

/// Chooses the view for values of `ty`: the first, in the set's order,
/// whose pattern names the type and which binds against it in `scope`.
pub fn choose<S: Scope>(views: &ViewSet, ty: TypeReference, scope: &S) -> Choice<S::Step> {
    let Ok((ty, info)) = representation(scope, ty) else {
        return Choice::default();
    };
    let Some(identity) = info.identity.as_deref() else {
        return Choice::default();
    };
    let mut choice = Choice::default();
    for &index in views
        .by_base
        .get(identity.base.as_ref())
        .into_iter()
        .flatten()
    {
        let view = &views.views[index];
        if !pattern::language_matches(view.language, identity.language) {
            continue;
        }
        let Some(captures) = pattern::matches(&view.pattern, identity, scope) else {
            continue;
        };
        match bind::bind(view, ty, &captures, scope) {
            Ok(bound) => {
                choice.candidates.push(Candidate {
                    name: name_of(view),
                    rejection: None,
                });
                choice.bound = Some(Arc::new(bound));
                return choice;
            }
            Err(rejection) => choice.candidates.push(Candidate {
                name: name_of(view),
                rejection: Some(rejection),
            }),
        }
    }
    choice
}
