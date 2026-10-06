//! The only ways the evaluator reaches a program: a [`Scope`] names things
//! while binding, and a [`Machine`] reads one stop while running.

use std::fmt;

use super::error::ErrorKind;
use super::interp::Value;
use super::number::Exact;
use super::syntax::ast::Tag;
use super::types::TypeSource;
use crate::{InspectedValue, TextSummary, TypeInfo, TypeReference, VariableState, VariableValue};

/// Why the debugger refuses part of an expression, without where; the
/// evaluator adds the span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub kind: ErrorKind,
    pub message: String,
}

impl Refusal {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// What a name means in a scope.
#[derive(Debug, Clone)]
pub enum Lookup<O> {
    /// A local, parameter, or global, with its type unless that is
    /// malformed.
    Object {
        object: O,
        ty: Result<TypeReference, std::sync::Arc<str>>,
    },
    /// An enumerator's value, typed by its enumeration.
    Enumerator {
        value: Exact,
        ty: TypeReference,
    },
    /// An exact integer the scope knows, such as an argument a view's
    /// pattern captured.
    Constant(Exact),
    /// A value the scope's machine computes once and keeps, of any type,
    /// such as a view's `let`: never a place.
    Bound {
        object: O,
        ty: super::types::Ty,
    },
    /// Several things the name could mean, each written as it is selected.
    Ambiguous(Vec<String>),
    NotFound,
}

/// A program type to look up, by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeQuery {
    /// The name as written, or a C base type's canonical spelling.
    pub name: String,
    /// The tag written before the name, as in `struct S`.
    pub tag: Option<Tag>,
}

/// What a type name means in a scope.
#[derive(Debug, Clone)]
pub enum TypeLookup {
    Found(TypeReference),
    Ambiguous(Vec<String>),
    NotFound,
}

/// One structural step a scope can plan.
#[derive(Debug, Clone, Copy)]
pub enum StepKind<'a> {
    /// Through a pointer or reference stored in program memory.
    Deref,
    /// To a member of a record, union, or variant.
    Member(&'a str),
    /// To an element of an array or slice, holding `available` indices.
    Index { available: usize },
    /// To the one base class subobject of a record of the given type.
    Base(TypeReference),
}

/// A step a scope planned from types alone.
#[derive(Debug, Clone)]
pub struct Planned<S> {
    pub step: S,
    /// The type the step reaches, or `None` when it reaches nothing usable.
    pub result: Option<TypeReference>,
    /// How many index values the step takes.
    pub consumed: usize,
}

/// A register a scope names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Register {
    /// The scope's own number for it.
    pub number: u16,
    /// Its width in bits.
    pub width: u8,
}

/// Bind-time view of one frame: what names mean there, and what types are.
pub trait Scope: TypeSource {
    /// A data object the scope can name.
    type Object: Clone + fmt::Debug;
    /// A planned structural step.
    type Step: Clone + fmt::Debug;

    /// What `name` means. A name of the outermost scope skips the frame's
    /// own locals.
    fn lookup(&self, name: &str, outermost: bool) -> Result<Lookup<Self::Object>, Refusal>;

    /// The program type a name means.
    fn lookup_type(&self, query: &TypeQuery) -> TypeLookup;

    /// Plans a step from a value of program type `from`.
    fn plan(&self, from: TypeReference, step: StepKind<'_>)
    -> Result<Planned<Self::Step>, Refusal>;

    /// The register a name, without its `$`, means.
    fn register(&self, name: &str) -> Option<Register>;

    /// Whether a view presents values of `ty` with a length, as one
    /// presents a Go map, which is a pointer.
    fn has_view(&self, ty: TypeReference) -> bool {
        let _ = ty;
        false
    }

    /// Whether `ty`, a pointer, only stands for a container its language
    /// gives a kind of its own, such as a Go map or channel: it is indexed,
    /// measured, and sized through the view that presents the container,
    /// never as a pointer, whether or not views are on.
    fn stands_for_container(&self, ty: TypeReference) -> bool {
        let _ = ty;
        false
    }

    /// The types whose identity has this base, in a stable order, for
    /// views that construct a type from its arguments.
    fn types_with_base(&self, base: &str) -> Vec<TypeReference> {
        let _ = base;
        Vec::new()
    }

    /// The global `name` of the module a view presents a value of, for a
    /// view's `global(NAME)`; nothing for any other scope.
    fn global(&self, name: &str) -> Result<Lookup<Self::Object>, Refusal> {
        let _ = name;
        Ok(Lookup::NotFound)
    }

    /// The global `name` of the module whose types this scope binds views
    /// against, as a step that reaches it from anywhere, and its type.
    fn global_step(&self, name: &str) -> Result<Option<(Self::Step, TypeReference)>, Refusal> {
        let _ = name;
        Ok(None)
    }
}

/// Why running stopped short of a value.
#[derive(Debug)]
pub enum Stop {
    /// The program state cannot provide the value: an unavailable or
    /// malformed state.
    Missing(Box<VariableState>),
    /// The expression asks for something the debugger refuses.
    Refused(Refusal),
    /// The debugger itself failed.
    Failed(crate::Error),
}

impl Stop {
    /// The program state cannot provide the value.
    pub fn missing(state: VariableState) -> Self {
        Self::Missing(Box::new(state))
    }
}

impl From<Refusal> for Stop {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

/// Run-time access to one validated stop.
pub trait Machine: TypeSource {
    type Object;
    type Step;
    /// Where a program-typed value is.
    type Place: Clone + fmt::Debug;

    /// Accounts one unit of evaluation work.
    fn charge(&mut self) -> Result<(), Stop>;

    fn locate(&mut self, object: &Self::Object) -> Result<Self::Place, Stop>;

    /// Checks index values against static bounds, before any storage is
    /// read.
    fn check_indices(&self, step: &Self::Step, indices: &[i128]) -> Result<(), Stop>;

    fn step(
        &mut self,
        from: &Self::Place,
        step: &Self::Step,
        indices: &[i128],
    ) -> Result<Self::Place, Stop>;

    /// The place of a value of `ty` in memory at `address`.
    fn place_at(&mut self, address: u64, ty: TypeReference) -> Result<Self::Place, Stop>;

    /// The address a place is stored at, refused when it is in a register
    /// or computed.
    fn address(&self, at: &Self::Place) -> Result<u64, Stop>;

    /// Decodes the value at a place of scalar type.
    fn load(&mut self, at: &Self::Place) -> Result<VariableValue, Stop>;

    /// Reads memory the expression addresses directly.
    fn read(&mut self, address: u64, size: usize) -> Result<Vec<u8>, Stop>;

    /// The text a place holds, or `None` when it is not text.
    fn text(&mut self, at: &Self::Place) -> Result<Option<TextSummary>, Stop>;

    /// A slice's run-time length.
    fn length(&mut self, at: &Self::Place) -> Result<u64, Stop>;

    /// A register's value, zero-extended.
    fn register(&mut self, register: &Register) -> Result<u128, Stop>;

    /// The value at a place, as inspection presents it.
    fn present(&mut self, at: &Self::Place) -> Result<InspectedValue, Stop>;

    /// A computed value of program type, from its bytes.
    fn present_bytes(&mut self, ty: TypeReference, bytes: &[u8]) -> Result<InspectedValue, Stop>;

    /// A computed pointer to `pointee`, which dereferences.
    fn present_pointer(
        &mut self,
        address: u64,
        pointee: Option<TypeReference>,
        type_info: TypeInfo,
    ) -> Result<InspectedValue, Stop>;

    /// The inspection's completion and usage so far, for results the
    /// evaluator builds itself.
    fn finish(&self, type_info: Option<TypeInfo>, state: VariableState) -> InspectedValue;

    /// How many elements a value with no length of its own holds, or the
    /// length of its text, as a view presents it; `None` when no view
    /// presents its type.
    fn presented_length(&mut self, at: &Self::Place) -> Result<Option<u64>, Stop> {
        let _ = at;
        Ok(None)
    }

    /// The value a scope bound to `object` with [`Lookup::Bound`].
    fn bound(&mut self, object: &Self::Object) -> Result<Value<Self::Place>, Stop> {
        let _ = object;
        Err(Stop::Refused(Refusal::new(
            ErrorKind::Unsupported,
            "this machine computes no bound values",
        )))
    }
}
