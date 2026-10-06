//! Frame and variable references handed to the client.
//!
//! Every reference comes from one session-wide counter that never resets,
//! so a reference from an earlier stop can never name an object of a later
//! one: once the inferior resumes, every reference is dropped and requests
//! naming one fail instead of being reinterpreted.

use std::collections::HashMap;
use std::sync::Arc;

use uscope::{
    DereferenceReference, StackFrameId, StopContext, ThreadId, ValueChildrenReference, VariableKind,
};

/// The largest reference DAP clients accept: references are 32-bit signed.
const MAX_REFERENCE: i64 = i32::MAX as i64;

/// What a variables reference expands to.
#[derive(Debug, Clone)]
pub enum Variables {
    /// The parameters or the locals of a frame.
    Scope {
        context: StopContext,
        kind: VariableKind,
    },
    /// A frame's registers.
    Registers { context: StopContext },
    /// The static variables declared in a frame's source file.
    Statics {
        context: StopContext,
        module: uscope::ModuleId,
        file: uscope::SourceFileId,
    },
    /// The children of an aggregate, named by `path` when it has a name.
    Children {
        context: StopContext,
        reference: Arc<ValueChildrenReference>,
        path: Option<uscope::Expression>,
        /// Whether the children are all elements, all named members, or
        /// (`None`) a view's elements followed by its named children.
        indexed: Option<bool>,
    },
    /// What a pointer or reference refers to.
    Pointee {
        context: StopContext,
        reference: DereferenceReference,
        name: Arc<str>,
        path: Option<uscope::Expression>,
    },
    /// The elements of an evaluated range, such as `values[2..6]`.
    Range {
        context: StopContext,
        expression: uscope::Expression,
    },
}

impl Variables {
    /// Whether the list's rows are elements, which a client asks for with
    /// the `indexed` filter, rather than named rows, which it asks for with
    /// `named`. `None` when a client has no count to page by, or when the
    /// list holds both, as a view's does.
    pub const fn indexed(&self) -> Option<bool> {
        match self {
            Self::Scope { .. } | Self::Registers { .. } | Self::Statics { .. } => Some(false),
            Self::Children { indexed, .. } => *indexed,
            Self::Range { .. } => Some(true),
            Self::Pointee { .. } => None,
        }
    }

    /// The frame whose values the reference expands.
    pub const fn context(&self) -> StopContext {
        match self {
            Self::Scope { context, .. }
            | Self::Registers { context }
            | Self::Statics { context, .. }
            | Self::Children { context, .. }
            | Self::Pointee { context, .. }
            | Self::Range { context, .. } => *context,
        }
    }
}

/// The references valid at the current stop.
#[derive(Debug)]
pub struct References {
    next: i64,
    frames: HashMap<i64, StopContext>,
    frame_ids: HashMap<(ThreadId, StackFrameId), i64>,
    variables: HashMap<i64, Variables>,
    /// The expression of each named row of a variables list, by the list's
    /// reference and the row's name.
    paths: HashMap<(i64, String), (StopContext, uscope::Expression)>,
    /// What each location reference names.
    locations: HashMap<i64, Location>,
}

/// A place in the program's source a location reference names.
#[derive(Debug, Clone)]
pub enum Location {
    /// The code at an address, such as the function a pointer points to.
    Code(u64),
    /// Where a module's debug information says something is declared.
    Declared {
        module: uscope::ModuleId,
        location: uscope::SourceLocation,
    },
}

/// The session ran out of references, which ends it rather than reusing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the session used every one of its {MAX_REFERENCE} references")]
pub struct Exhausted;

impl Default for References {
    fn default() -> Self {
        Self {
            next: 1,
            frames: HashMap::new(),
            frame_ids: HashMap::new(),
            variables: HashMap::new(),
            paths: HashMap::new(),
            locations: HashMap::new(),
        }
    }
}

impl References {
    const fn allocate(&mut self) -> Result<i64, Exhausted> {
        if self.next > MAX_REFERENCE {
            return Err(Exhausted);
        }
        let id = self.next;
        self.next += 1;
        Ok(id)
    }

    /// Returns the reference of a frame, the same one each time it is asked
    /// for during one stop.
    pub fn frame(&mut self, context: StopContext) -> Result<i64, Exhausted> {
        if let Some(&id) = self.frame_ids.get(&(context.thread, context.frame)) {
            return Ok(id);
        }
        let id = self.allocate()?;
        self.frames.insert(id, context);
        self.frame_ids.insert((context.thread, context.frame), id);
        Ok(id)
    }

    /// Returns a new reference that names nothing, such as for a row that
    /// only labels.
    pub const fn label(&mut self) -> Result<i64, Exhausted> {
        self.allocate()
    }

    /// Returns a new reference for something expandable.
    pub fn variables(&mut self, variables: Variables) -> Result<i64, Exhausted> {
        let id = self.allocate()?;
        self.variables.insert(id, variables);
        Ok(id)
    }

    /// Returns a new reference to a place in the program's source.
    pub fn location(&mut self, location: Location) -> Result<i64, Exhausted> {
        let id = self.allocate()?;
        self.locations.insert(id, location);
        Ok(id)
    }

    /// The place a location reference names.
    pub fn location_of(&self, id: i64) -> Option<&Location> {
        self.locations.get(&id)
    }

    pub fn frame_context(&self, id: i64) -> Option<StopContext> {
        self.frames.get(&id).copied()
    }

    pub fn variables_of(&self, id: i64) -> Option<&Variables> {
        self.variables.get(&id)
    }

    /// Records the expression a row of a variables list evaluates as.
    pub fn record_path(
        &mut self,
        list: i64,
        name: String,
        context: StopContext,
        path: uscope::Expression,
    ) {
        self.paths.insert((list, name), (context, path));
    }

    /// The expression of a row of a variables list, with its frame: as it
    /// was listed, or else as the list's kind names its rows.
    pub fn child_path(&self, list: i64, name: &str) -> Option<(StopContext, uscope::Expression)> {
        if let Some(recorded) = self.paths.get(&(list, name.to_owned())) {
            return Some(recorded.clone());
        }
        // A row is named `[i]`, `[i][j]`, or by its member's name.
        let row = |parent: &uscope::Expression, name: &str| {
            if name.starts_with('[') {
                let indices: Option<Vec<i128>> = name
                    .strip_prefix('[')?
                    .strip_suffix(']')?
                    .split("][")
                    .map(|index| index.parse().ok())
                    .collect();
                parent.indexed(&indices?)
            } else {
                parent.member(name)
            }
        };
        match self.variables.get(&list)? {
            Variables::Scope { context, .. } => Some((*context, uscope::Expression::name(name)?)),
            Variables::Children {
                context,
                path: Some(path),
                ..
            } => Some((*context, row(path, name)?)),
            Variables::Pointee {
                context,
                name: owner,
                path: Some(path),
                ..
            } => {
                let pointee = path.dereferenced()?;
                if name == format!("*{owner}") {
                    Some((*context, pointee))
                } else {
                    Some((*context, row(&pointee, name)?))
                }
            }
            Variables::Range {
                context,
                expression,
            } => Some((*context, row(&expression.range_base()?, name)?)),
            _ => None,
        }
    }

    /// Drops every reference, as when the inferior resumes.
    pub fn clear(&mut self) {
        self.frames.clear();
        self.frame_ids.clear();
        self.variables.clear();
        self.paths.clear();
        self.locations.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uscope::StopId;

    fn context(stop: u64, thread: u64) -> StopContext {
        StopContext {
            stop: StopId::new(stop),
            thread: ThreadId::new(thread),
            frame: StackFrameId::INNERMOST,
        }
    }

    #[test]
    fn references_are_stable_within_a_stop_and_never_reused_after_it() {
        let mut references = References::default();
        let first = references.frame(context(1, 7)).expect("reference");
        assert_eq!(references.frame(context(1, 7)), Ok(first));
        let scope = references
            .variables(Variables::Registers {
                context: context(1, 7),
            })
            .expect("reference");
        assert!(first > 0 && scope > first);

        references.clear();
        assert_eq!(references.frame_context(first), None);
        assert!(references.variables_of(scope).is_none());
        let again = references.frame(context(2, 7)).expect("reference");
        assert!(again > scope, "a dropped reference is never reused");
        assert_eq!(references.frame_context(again), Some(context(2, 7)));
    }

    #[test]
    fn exhaustion_is_an_error_rather_than_a_wrap_around() {
        let mut references = References {
            next: MAX_REFERENCE,
            ..References::default()
        };
        assert_eq!(references.frame(context(1, 1)), Ok(MAX_REFERENCE));
        assert_eq!(references.frame(context(1, 2)), Err(Exhausted));
        assert_eq!(
            references
                .variables(Variables::Registers {
                    context: context(1, 1)
                })
                .err(),
            Some(Exhausted)
        );
    }
}
