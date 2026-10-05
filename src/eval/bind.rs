//! Binding: resolving an expression's names and types in one scope into a
//! typed program, and refusing what its types do not allow.

use std::sync::Arc;

use super::error::{ErrorKind, ExpressionError};
use super::ir::{Comparison, Constant, Conversion, Length, Node, Op, Program};
use super::number::{
    BitOperator, Bits, Exact, Float, FloatFormat, FloatOperator, IntType, Integer,
};
use super::syntax::ast::{
    BinaryOp, Field, NodeId, NodeKind, Path, Separator, SizeOf, Suffix, Tree, TypeBase, TypeName,
    UnaryOp,
};
use super::syntax::{Expression, Span};
use super::target::{Lookup, Refusal, Scope, StepKind, TypeLookup, TypeQuery};
use super::types::{Category, Ty, builtin, c_type_key, category, is_character, size_of, type_name};
use crate::TypeReference;

/// Whether an expression may assign.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Reading only: hovers, watches, conditions, and log messages.
    Read,
    /// Assigning too: the console and `set var`.
    Assign,
}

/// The most nodes a bound program may hold.
const MAX_BOUND_NODES: usize = 4096;

type Bound<S> = Node<<S as Scope>::Object, <S as Scope>::Step>;
type BindResult<S> = Result<Bound<S>, ExpressionError>;

/// Binds `expression` in `scope`, choosing for each ambiguity the reading
/// its name has there.
pub fn bind<S: Scope>(
    expression: &Expression,
    scope: &S,
    mode: Mode,
) -> Result<Program<S::Object, S::Step>, ExpressionError> {
    bind_as(expression, scope, mode, false)
}

/// Binds a breakpoint's condition, whose value is its truth.
pub fn bind_condition<S: Scope>(
    expression: &Expression,
    scope: &S,
) -> Result<Program<S::Object, S::Step>, ExpressionError> {
    bind_as(expression, scope, Mode::Read, true)
}

fn bind_as<S: Scope>(
    expression: &Expression,
    scope: &S,
    mode: Mode,
    truth: bool,
) -> Result<Program<S::Object, S::Step>, ExpressionError> {
    let mut casts = 0;
    for (index, ambiguity) in expression.ambiguities().iter().enumerate() {
        let binder = Binder::new(scope, expression.text(), mode, None);
        if binder.names_value(&ambiguity.name)? {
            continue;
        }
        let ty = TypeName {
            base: TypeBase::Named(ambiguity.name.clone()),
            pointers: 0,
            span: ambiguity.span,
        };
        if binder.resolve_type(&ty).is_ok() {
            casts |= 1 << index;
        } else {
            return Err(ExpressionError::new(
                ErrorKind::UnknownName,
                ambiguity.span,
                format!(
                    "`{}` names neither a value nor a type",
                    super::syntax::print_path(&ambiguity.name)
                ),
            ));
        }
    }
    let tree = expression.reading(casts).map_err(Clone::clone)?;
    let mut binder = Binder::new(scope, expression.text(), mode, Some(tree));
    let root = match tree.kind(tree.root()) {
        NodeKind::Assign { op, target, value } if mode == Mode::Assign => {
            binder.assignment(*op, *target, *value, tree.span(tree.root()))?
        }
        _ => binder.bind(tree.root())?,
    };
    let root = if matches!(root.op, Op::Range { .. } | Op::Assign { .. }) {
        root
    } else {
        binder.settle(root)?
    };
    let root = if truth { binder.truth(root)? } else { root };
    if matches!(root.ty, Ty::Text) {
        return Err(ExpressionError::new(
            ErrorKind::Type,
            root.span,
            "a string is only compared with the program's text",
        ));
    }
    Ok(Program { root })
}

struct Binder<'a, S: Scope> {
    scope: &'a S,
    text: &'a str,
    mode: Mode,
    tree: Option<&'a Tree>,
    nodes: usize,
}

/// A failure that is about an operand's type being one the debugger cannot
/// compute with, rather than the expression being wrong.
fn opaque(category: &Category) -> Option<&str> {
    match category {
        Category::Opaque(reason) => Some(reason),
        _ => None,
    }
}

impl<'a, S: Scope> Binder<'a, S> {
    const fn new(scope: &'a S, text: &'a str, mode: Mode, tree: Option<&'a Tree>) -> Self {
        Self {
            scope,
            text,
            mode,
            tree,
            nodes: 0,
        }
    }

    const fn tree(&self) -> &'a Tree {
        self.tree.expect("binding a reading")
    }

    fn error(span: Span, kind: ErrorKind, message: impl Into<String>) -> ExpressionError {
        ExpressionError::new(kind, span, message)
    }

    fn refused(span: Span, refusal: Refusal) -> ExpressionError {
        ExpressionError::new(refusal.kind, span, refusal.message)
    }

    /// The text a span covers, for messages.
    fn quote(&self, span: Span) -> &str {
        span.text(self.text)
    }

    fn node(&mut self, op: Op<S::Object, S::Step>, ty: Ty, span: Span) -> BindResult<S> {
        self.nodes += 1;
        if self.nodes > MAX_BOUND_NODES {
            return Err(Self::error(
                span,
                ErrorKind::Limit,
                "the expression binds to too many operations",
            ));
        }
        Ok(Node { op, ty, span })
    }

    fn category(&self, ty: &Ty) -> Category {
        category(self.scope, ty)
    }

    #[expect(clippy::too_many_lines, reason = "one arm per kind of syntax node")]
    fn bind(&mut self, id: NodeId) -> BindResult<S> {
        let span = self.tree().span(id);
        match self.tree().kind(id).clone() {
            NodeKind::Name(path) => self.name(&path, span),
            NodeKind::Register(name) => {
                let register = self.scope.register(&name).ok_or_else(|| {
                    Self::error(
                        span,
                        ErrorKind::UnknownName,
                        format!("there is no register `${name}`"),
                    )
                })?;
                let ty = Ty::Int(
                    IntType::new(register.width, false)
                        .unwrap_or_else(|| IntType::new(64, false).expect("64 is a valid width")),
                );
                self.node(Op::Register(register), ty, span)
            }
            NodeKind::Integer { value, suffix } => self.integer(value, suffix, false, span),
            NodeKind::Float(value) => self.node(
                Op::Constant(Constant::Float(value)),
                Ty::Float(value.format()),
                span,
            ),
            NodeKind::Char(value) => self.node(
                Op::Constant(Constant::Integer(Integer::Exact(Exact::from(u128::from(
                    value,
                ))))),
                Ty::Exact,
                span,
            ),
            NodeKind::Text(bytes) => self.node(Op::Constant(Constant::Text(bytes)), Ty::Text, span),
            NodeKind::Bool(value) => self.node(Op::Constant(Constant::Bool(value)), Ty::Bool, span),
            NodeKind::Null => self.node(Op::Constant(Constant::Pointer(0)), Ty::Null, span),
            NodeKind::Unary { op, operand } => self.unary(op, operand, span),
            NodeKind::Binary { op, left, right } => self.binary(op, left, right, span),
            NodeKind::Conditional {
                condition,
                then,
                otherwise,
            } => self.conditional(condition, then, otherwise, span),
            NodeKind::Assign { .. } => match self.mode {
                Mode::Read => Err(Self::error(
                    span,
                    ErrorKind::Mode,
                    "assignments are only made in the console and with `set var`",
                )),
                Mode::Assign => Err(Self::error(
                    span,
                    ErrorKind::Mode,
                    "an assignment must be the whole expression",
                )),
            },
            NodeKind::Cast { ty, operand, .. } => {
                let target = self.resolve_type(&ty)?;
                let operand = self.bind(operand)?;
                self.convert(operand, target, span)
            }
            NodeKind::Member {
                base,
                field,
                field_span,
                arrow,
            } => {
                // `a.b.c` may name a global, as Go's `main.counter` does: the
                // longest name the scope knows is taken first.
                if let Some(path) = self.dotted(id) {
                    return self.name(&path, span);
                }
                let base = self.bind(base)?;
                self.member(base, &field, field_span, arrow, span)
            }
            NodeKind::Index { .. } => self.index(id),
            NodeKind::Range { base, start, end } => {
                let base = self.bind(base)?;
                let base = self.settle(base)?;
                if !base.is_place()
                    || !matches!(
                        self.category(&base.ty),
                        Category::Array { .. } | Category::Slice(_)
                    )
                {
                    return Err(Self::error(
                        base.span,
                        ErrorKind::Type,
                        format!("`{}` is not an array or slice", self.quote(base.span)),
                    ));
                }
                let start = self.bind(start)?;
                let start = self.integer_value(start)?;
                let end = self.bind(end)?;
                let end = self.integer_value(end)?;
                let ty = base.ty.clone();
                self.node(
                    Op::Range {
                        base: Box::new(base),
                        start: Box::new(start),
                        end: Box::new(end),
                    },
                    ty,
                    span,
                )
            }
            NodeKind::SizeOf(SizeOf::Type(ty)) => {
                let ty = self.resolve_type(&ty)?;
                self.size_constant(&ty, span)
            }
            NodeKind::SizeOf(SizeOf::Operand(operand)) => {
                if let NodeKind::Name(path) = self.tree().kind(operand).clone()
                    && !self.names_value(&path)?
                {
                    let name = TypeName {
                        base: TypeBase::Named(path),
                        pointers: 0,
                        span: self.tree().span(operand),
                    };
                    let ty = self.resolve_type(&name)?;
                    return self.size_constant(&ty, span);
                }
                let operand = self.bind(operand)?;
                let operand = self.settle(operand)?;
                self.size_constant(&operand.ty, span)
            }
            NodeKind::Len(operand) => {
                let operand = self.bind(operand)?;
                self.length(operand, span)
            }
        }
    }

    // ---- Names ----

    /// A chain of `.name` selections from a name, as one dotted path.
    fn dotted(&self, id: NodeId) -> Option<Path> {
        match self.tree().kind(id) {
            NodeKind::Name(path) => Some(path.clone()),
            NodeKind::Member {
                base,
                field: Field::Named(name),
                field_span,
                arrow: false,
            } => {
                let mut path = self.dotted(*base)?;
                path.segments.push(super::syntax::ast::Segment {
                    separator: Separator::Dot,
                    name: name.clone(),
                    span: *field_span,
                });
                Some(path)
            }
            _ => None,
        }
    }

    /// The text of a path's first `count` segments.
    fn prefix_text(path: &Path, count: usize) -> String {
        let mut text = String::new();
        for (index, segment) in path.segments.iter().take(count).enumerate() {
            if index > 0 {
                text.push_str(match segment.separator {
                    Separator::Colons => "::",
                    Separator::Dot => ".",
                });
            }
            text.push_str(&segment.name);
        }
        text
    }

    /// Where a path may split into a name and member selections: before
    /// each `.`, longest name first.
    fn splits(path: &Path) -> impl Iterator<Item = usize> + '_ {
        (1..=path.segments.len()).rev().filter(|&count| {
            path.segments
                .get(count)
                .is_none_or(|next| next.separator == Separator::Dot)
        })
    }

    /// Whether a path names a value, possibly through members.
    fn names_value(&self, path: &Path) -> Result<bool, ExpressionError> {
        for count in Self::splits(path) {
            let span = path.segments[0].span.to(path.segments[count - 1].span);
            match self
                .scope
                .lookup(&Self::prefix_text(path, count), path.global)
                .map_err(|refusal| Self::refused(span, refusal))?
            {
                Lookup::NotFound => {}
                _ => return Ok(true),
            }
        }
        Ok(false)
    }

    fn name(&mut self, path: &Path, span: Span) -> BindResult<S> {
        for count in Self::splits(path) {
            let name = Self::prefix_text(path, count);
            let name_span = path.segments[0].span.to(path.segments[count - 1].span);
            let found = self
                .scope
                .lookup(&name, path.global)
                .map_err(|refusal| Self::refused(name_span, refusal))?;
            let mut node = match found {
                Lookup::NotFound => continue,
                Lookup::Ambiguous(candidates) => {
                    return Err(Self::error(
                        name_span,
                        ErrorKind::AmbiguousName,
                        format!(
                            "`{name}` is ambiguous; select one of {}",
                            candidates
                                .iter()
                                .map(|candidate| format!("`{candidate}`"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    ));
                }
                Lookup::Enumerator { value, ty } => {
                    if count < path.segments.len() {
                        return Err(Self::error(
                            span,
                            ErrorKind::Type,
                            format!("the enumerator `{name}` has no members"),
                        ));
                    }
                    return self.enumerator(value, ty, span);
                }
                Lookup::Object { object, ty } => {
                    let ty = match ty {
                        Ok(ty) => ty,
                        Err(reason) => {
                            return Err(Self::error(
                                name_span,
                                ErrorKind::Unsupported,
                                format!("`{name}` has a malformed type: {reason}"),
                            ));
                        }
                    };
                    self.node(Op::Object(object), Ty::Program(ty), name_span)?
                }
            };
            for segment in &path.segments[count..] {
                let member_span = path.segments[0].span.to(segment.span);
                node = self.member(
                    node,
                    &Field::Named(segment.name.clone()),
                    segment.span,
                    false,
                    member_span,
                )?;
            }
            return Ok(node);
        }
        let name = Self::prefix_text(path, path.segments.len());
        Err(Self::error(
            span,
            ErrorKind::UnknownName,
            format!("no variable is named `{name}` here"),
        ))
    }

    fn enumerator(&mut self, value: Exact, ty: TypeReference, span: Span) -> BindResult<S> {
        let ty = Ty::Program(ty);
        let integer = match self.category(&ty) {
            Category::Integer { int: Some(int), .. } => Integer::Typed(Bits::truncate(int, value)),
            _ => Integer::Exact(value),
        };
        self.node(Op::Constant(Constant::Integer(integer)), ty, span)
    }

    // ---- Literals ----

    fn integer(
        &mut self,
        value: u128,
        suffix: Option<Suffix>,
        negated: bool,
        span: Span,
    ) -> BindResult<S> {
        let exact = Exact::from(value);
        let exact = if negated {
            exact
                .neg()
                .map_err(|error| Self::error(span, ErrorKind::Arithmetic, error.to_string()))?
        } else {
            exact
        };
        let int = match suffix {
            None => {
                return self.node(
                    Op::Constant(Constant::Integer(Integer::Exact(exact))),
                    Ty::Exact,
                    span,
                );
            }
            Some(Suffix::Int { width, signed }) => IntType::new(width, signed),
            Some(Suffix::Size { signed }) => {
                IntType::new(self.scope.pointer_size().saturating_mul(8), signed)
            }
            Some(Suffix::F32 | Suffix::F64) => unreachable!("float suffixes make float literals"),
        };
        let int =
            int.ok_or_else(|| Self::error(span, ErrorKind::Syntax, "the suffix names no type"))?;
        let bits = Bits::exactly(int, exact).map_err(|_| {
            Self::error(
                span,
                ErrorKind::Arithmetic,
                format!(
                    "{exact} does not fit {}",
                    type_name(self.scope, &Ty::Int(int))
                ),
            )
        })?;
        self.node(
            Op::Constant(Constant::Integer(Integer::Typed(bits))),
            Ty::Int(int),
            span,
        )
    }

    // ---- Types ----

    /// The type a type name means: a built-in, then the program's.
    fn resolve_type(&self, name: &TypeName) -> Result<Ty, ExpressionError> {
        let span = name.span;
        let query = match &name.base {
            TypeBase::Named(path) => {
                if let [segment] = path.segments.as_slice()
                    && !path.global
                    && let Some(ty) = builtin(&segment.name, self.scope.pointer_size())
                {
                    return Ok(Self::pointers(ty, name.pointers));
                }
                TypeQuery {
                    name: Self::prefix_text(path, path.segments.len()),
                    tag: None,
                }
            }
            TypeBase::CWords(words) => TypeQuery {
                name: c_type_key(words).ok_or_else(|| {
                    Self::error(span, ErrorKind::UnknownName, "the words spell no C type")
                })?,
                tag: None,
            },
            TypeBase::Tagged(tag, path) => TypeQuery {
                name: Self::prefix_text(path, path.segments.len()),
                tag: Some(*tag),
            },
        };
        match self.scope.lookup_type(&query) {
            TypeLookup::Found(ty) => Ok(Self::pointers(Ty::Program(ty), name.pointers)),
            TypeLookup::Ambiguous(candidates) => Err(Self::error(
                span,
                ErrorKind::AmbiguousName,
                format!(
                    "several types are named `{}`: {}",
                    query.name,
                    candidates.join(", ")
                ),
            )),
            TypeLookup::NotFound => Err(Self::error(
                span,
                ErrorKind::UnknownName,
                format!("no type is named `{}`", query.name),
            )),
        }
    }

    fn pointers(mut ty: Ty, count: u8) -> Ty {
        for _ in 0..count {
            ty = Ty::Pointer(Arc::new(ty));
        }
        ty
    }

    fn size_constant(&mut self, ty: &Ty, span: Span) -> BindResult<S> {
        let size = size_of(self.scope, ty).ok_or_else(|| {
            Self::error(
                span,
                ErrorKind::Type,
                format!("`{}` has no size", type_name(self.scope, ty)),
            )
        })?;
        self.node(
            Op::Constant(Constant::Integer(Integer::Exact(Exact::from(u128::from(
                size,
            ))))),
            Ty::Exact,
            span,
        )
    }

    // ---- Places and values ----

    /// A language reference stands for what it refers to.
    fn settle(&mut self, node: Bound<S>) -> BindResult<S> {
        if let Category::Reference(_) = self.category(&node.ty) {
            return self.deref_place(node);
        }
        Ok(node)
    }

    /// Follows a place holding a pointer or reference, with the provider's
    /// own step, which knows implicit pointers and address classes.
    fn deref_place(&mut self, node: Bound<S>) -> BindResult<S> {
        let span = node.span;
        let Ty::Program(from) = node.ty else {
            unreachable!("only program types are stored in places")
        };
        let planned = self
            .scope
            .plan(from, StepKind::Deref)
            .map_err(|refusal| Self::refused(span, refusal))?;
        let ty = planned.result.map_or_else(
            || {
                Err(Self::error(
                    span,
                    ErrorKind::Type,
                    format!("`{}` points at nothing usable", self.quote(span)),
                ))
            },
            |ty| Ok(Ty::Program(ty)),
        )?;
        self.node(
            Op::Step {
                base: Box::new(node),
                step: planned.step,
                indices: Vec::new(),
                follows: true,
            },
            ty,
            span,
        )
    }

    /// The place a pointer value points at.
    fn deref_value(&mut self, pointer: Bound<S>, target: &Ty, span: Span) -> BindResult<S> {
        match target {
            Ty::Program(reference) => self.node(
                Op::At {
                    address: Box::new(pointer),
                    pointee: *reference,
                },
                target.clone(),
                span,
            ),
            Ty::Void => Err(Self::error(
                pointer.span,
                ErrorKind::Type,
                format!("`{}` points at void", self.quote(pointer.span)),
            )
            .with_hint("cast it to a pointer to the type it points at")),
            other => self.node(
                Op::Raw {
                    address: Box::new(pointer),
                },
                other.clone(),
                span,
            ),
        }
    }

    /// Dereferences a pointer-typed node, stored or computed.
    fn deref(&mut self, node: Bound<S>, span: Span) -> BindResult<S> {
        let node = self.settle(node)?;
        match self.category(&node.ty) {
            Category::Pointer(Some(_)) if node.is_place() && matches!(node.ty, Ty::Program(_)) => {
                let mut stepped = self.deref_place(node)?;
                stepped.span = span;
                Ok(stepped)
            }
            Category::Pointer(Some(pointee)) => {
                let value = self.value(node)?;
                self.deref_value(value, &pointee, span)
            }
            Category::Pointer(None) => Err(Self::error(
                node.span,
                ErrorKind::Type,
                format!("`{}` points at void", self.quote(node.span)),
            )
            .with_hint("cast it to a pointer to the type it points at")),
            Category::Array { .. } => {
                // `*array` is its first element.
                let pointer = self.operand(node)?;
                let Ty::Pointer(element) = pointer.ty.clone() else {
                    unreachable!("an array decays to a pointer")
                };
                self.deref_value(pointer, &element, span)
            }
            category => Err(self.type_error(&node, &category, "is not a pointer")),
        }
    }

    fn type_error(&self, node: &Bound<S>, category: &Category, what: &str) -> ExpressionError {
        if let Some(reason) = opaque(category) {
            return Self::error(
                node.span,
                ErrorKind::Unsupported,
                format!(
                    "`{}` cannot be computed with: {reason}",
                    self.quote(node.span)
                ),
            );
        }
        Self::error(
            node.span,
            ErrorKind::Type,
            format!(
                "`{}` {what}; its type is `{}`",
                self.quote(node.span),
                type_name(self.scope, &node.ty)
            ),
        )
    }

    /// The value of a node: a place's scalar is loaded.
    fn value(&mut self, node: Bound<S>) -> BindResult<S> {
        let node = self.settle(node)?;
        if !node.is_place() {
            return Ok(node);
        }
        match self.category(&node.ty) {
            Category::Integer { .. }
            | Category::Float(_)
            | Category::Bool
            | Category::Pointer(_) => {
                let (ty, span) = (node.ty.clone(), node.span);
                self.node(Op::Load(Box::new(node)), ty, span)
            }
            Category::Opaque(reason) => Err(Self::error(
                node.span,
                ErrorKind::Unsupported,
                format!(
                    "`{}` cannot be computed with: {reason}",
                    self.quote(node.span)
                ),
            )),
            _ => Ok(node),
        }
    }

    /// An operand of arithmetic or comparison: arrays decay to a pointer to
    /// their first element.
    fn operand(&mut self, node: Bound<S>) -> BindResult<S> {
        let node = self.settle(node)?;
        if let Category::Array { element, .. } = self.category(&node.ty)
            && node.is_place()
        {
            let span = node.span;
            return self.node(
                Op::Decay(Box::new(node)),
                Ty::Pointer(Arc::new(Ty::Program(element))),
                span,
            );
        }
        self.value(node)
    }

    fn integer_value(&mut self, node: Bound<S>) -> BindResult<S> {
        let node = self.value(node)?;
        match self.category(&node.ty) {
            Category::Integer { .. } => Ok(node),
            category => Err(self.type_error(&node, &category, "is not an integer")),
        }
    }

    /// A truth value: booleans, or numbers and pointers, true when nonzero.
    fn truth(&mut self, node: Bound<S>) -> BindResult<S> {
        let node = self.operand(node)?;
        match self.category(&node.ty) {
            Category::Bool => Ok(node),
            Category::Integer { .. }
            | Category::Float(_)
            | Category::Pointer(_)
            | Category::Null => {
                let span = node.span;
                self.node(
                    Op::Convert {
                        operand: Box::new(node),
                        conversion: Conversion::Truth,
                    },
                    Ty::Bool,
                    span,
                )
            }
            category => Err(self.type_error(&node, &category, "has no truth value")),
        }
    }

    // ---- Operators ----

    fn unary(&mut self, op: UnaryOp, operand: NodeId, span: Span) -> BindResult<S> {
        if op == UnaryOp::Neg
            && let NodeKind::Integer { value, suffix } = self.tree().kind(operand).clone()
        {
            // A negative literal of a type, such as `-128i8`, is written as
            // the negation of a literal that alone would not fit.
            return self.integer(value, suffix, true, span);
        }
        let operand = self.bind(operand)?;
        match op {
            UnaryOp::Neg => {
                let operand = self.value(operand)?;
                match self.category(&operand.ty) {
                    Category::Integer { .. } => {
                        self.node(Op::Negate(Box::new(operand)), Ty::Exact, span)
                    }
                    Category::Float(format) => {
                        self.node(Op::FloatNegate(Box::new(operand)), Ty::Float(format), span)
                    }
                    category => Err(self.type_error(&operand, &category, "is not a number")),
                }
            }
            UnaryOp::Not => {
                let operand = self.truth(operand)?;
                self.node(Op::Not(Box::new(operand)), Ty::Bool, span)
            }
            UnaryOp::BitNot => {
                let operand = self.integer_value(operand)?;
                let ty = operand.ty.clone();
                self.node(Op::BitNot(Box::new(operand)), ty, span)
            }
            UnaryOp::Deref => self.deref(operand, span),
            UnaryOp::AddressOf => {
                let operand = self.settle(operand)?;
                if !operand.is_place() {
                    return Err(Self::error(
                        operand.span,
                        ErrorKind::NotAnLvalue,
                        format!(
                            "`{}` is a computed value, which has no address",
                            self.quote(operand.span)
                        ),
                    ));
                }
                let ty = Ty::Pointer(Arc::new(operand.ty.clone()));
                self.node(Op::AddressOf(Box::new(operand)), ty, span)
            }
        }
    }

    /// Binds an operand that may be an enumerator of the other operand's
    /// enumeration, found there when the scope finds no single value by its
    /// name.
    fn bind_beside(&mut self, id: NodeId, other: Option<&Bound<S>>) -> BindResult<S> {
        let bound = self.bind(id);
        let Err(error) = &bound else {
            return bound;
        };
        let (Some(other), NodeKind::Name(path)) = (other, self.tree().kind(id)) else {
            return bound;
        };
        if !matches!(
            error.kind,
            ErrorKind::UnknownName | ErrorKind::AmbiguousName
        ) || path.segments.len() != 1
            || path.global
        {
            return bound;
        }
        let Category::Integer {
            enumerators: Some(enumerators),
            ..
        } = self.category(&other.ty)
        else {
            return bound;
        };
        let Ty::Program(enumeration) = other.ty else {
            return bound;
        };
        let name = &path.segments[0].name;
        let Some(found) = enumerators
            .iter()
            .find(|enumerator| enumerator.name.as_ref() == name)
        else {
            return bound;
        };
        let value = match found.value {
            crate::IntegerValue::Signed(value) => Exact::from(value),
            crate::IntegerValue::Unsigned(value) => Exact::from(value),
        };
        self.enumerator(value, enumeration, self.tree().span(id))
    }

    /// Binds both operands of a binary operator, letting either name an
    /// enumerator of the other's enumeration.
    fn operands(
        &mut self,
        left: NodeId,
        right: NodeId,
    ) -> Result<(Bound<S>, Bound<S>), ExpressionError> {
        match self.bind(left) {
            Ok(left) => {
                let right = self.bind_beside(right, Some(&left))?;
                Ok((left, right))
            }
            Err(error) => {
                let Ok(right) = self.bind(right) else {
                    return Err(error);
                };
                let left = self.bind_beside(left, Some(&right))?;
                Ok((left, right))
            }
        }
    }

    fn binary(&mut self, op: BinaryOp, left: NodeId, right: NodeId, span: Span) -> BindResult<S> {
        if matches!(op, BinaryOp::And | BinaryOp::Or) {
            let left = self.bind(left)?;
            let left = self.truth(left)?;
            let right = self.bind(right)?;
            let right = self.truth(right)?;
            return self.node(
                Op::Logical {
                    and: op == BinaryOp::And,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                Ty::Bool,
                span,
            );
        }
        let (left, right) = self.operands(left, right)?;
        self.binary_bound(op, left, right, span)
    }

    /// A binary operator other than `&&` and `||` on bound operands.
    fn binary_bound(
        &mut self,
        op: BinaryOp,
        left: Bound<S>,
        right: Bound<S>,
        span: Span,
    ) -> BindResult<S> {
        match op {
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => {
                self.arithmetic(op, left, right, span)
            }
            BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor => {
                let left = self.integer_value(left)?;
                let right = self.integer_value(right)?;
                let ty = self.bitwise_type(&left.ty, &right.ty);
                let op = match op {
                    BinaryOp::BitAnd => BitOperator::And,
                    BinaryOp::BitOr => BitOperator::Or,
                    _ => BitOperator::Xor,
                };
                self.node(
                    Op::Bitwise {
                        op,
                        left: Box::new(left),
                        right: Box::new(right),
                    },
                    ty,
                    span,
                )
            }
            BinaryOp::Shl | BinaryOp::Shr => {
                let value = self.integer_value(left)?;
                let amount = self.integer_value(right)?;
                let ty = value.ty.clone();
                self.node(
                    Op::Shift {
                        left: op == BinaryOp::Shl,
                        value: Box::new(value),
                        amount: Box::new(amount),
                    },
                    ty,
                    span,
                )
            }
            _ => self.comparison(op, left, right, span),
        }
    }

    /// The type a bit operation keeps: the wider typed operand's, the
    /// unsigned one's at equal widths, the left's when both agree.
    fn bitwise_type(&self, left: &Ty, right: &Ty) -> Ty {
        let int = |ty: &Ty| match self.category(ty) {
            Category::Integer { int, .. } => int,
            _ => None,
        };
        match (int(left), int(right)) {
            (None, None) => Ty::Exact,
            (Some(_), None) => left.clone(),
            (None, Some(_)) => right.clone(),
            (Some(left_int), Some(right_int)) => {
                let common = left_int.common(right_int);
                if common == left_int {
                    left.clone()
                } else if common == right_int {
                    right.clone()
                } else {
                    Ty::Int(common)
                }
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per pair of operand categories"
    )]
    fn arithmetic(
        &mut self,
        op: BinaryOp,
        left: Bound<S>,
        right: Bound<S>,
        span: Span,
    ) -> BindResult<S> {
        let left = self.operand(left)?;
        let right = self.operand(right)?;
        let (left_category, right_category) = (self.category(&left.ty), self.category(&right.ty));
        match (&left_category, &right_category) {
            (Category::Integer { .. }, Category::Integer { .. }) => self.node(
                Op::Arithmetic {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                Ty::Exact,
                span,
            ),
            (
                Category::Float(_) | Category::Integer { .. },
                Category::Float(_) | Category::Integer { .. },
            ) => {
                let format = [&left_category, &right_category]
                    .into_iter()
                    .filter_map(|category| match category {
                        Category::Float(format) => Some(*format),
                        _ => None,
                    })
                    .max()
                    .expect("one operand is a float");
                let left = self.float_operand(left, format)?;
                let right = self.float_operand(right, format)?;
                let op = match op {
                    BinaryOp::Add => FloatOperator::Add,
                    BinaryOp::Sub => FloatOperator::Sub,
                    BinaryOp::Mul => FloatOperator::Mul,
                    BinaryOp::Div => FloatOperator::Div,
                    _ => FloatOperator::Rem,
                };
                self.node(
                    Op::FloatArithmetic {
                        op,
                        left: Box::new(left),
                        right: Box::new(right),
                    },
                    Ty::Float(format),
                    span,
                )
            }
            (Category::Pointer(pointee), Category::Integer { .. })
                if matches!(op, BinaryOp::Add | BinaryOp::Sub) =>
            {
                let scale = self.element_size(&left, pointee.as_ref())?;
                let ty = left.ty.clone();
                self.node(
                    Op::Offset {
                        pointer: Box::new(left),
                        count: Box::new(right),
                        scale,
                        backward: op == BinaryOp::Sub,
                    },
                    ty,
                    span,
                )
            }
            (Category::Integer { .. }, Category::Pointer(pointee)) if op == BinaryOp::Add => {
                let scale = self.element_size(&right, pointee.as_ref())?;
                let ty = right.ty.clone();
                self.node(
                    Op::Offset {
                        pointer: Box::new(right),
                        count: Box::new(left),
                        scale,
                        backward: false,
                    },
                    ty,
                    span,
                )
            }
            (Category::Pointer(left_pointee), Category::Pointer(right_pointee))
                if op == BinaryOp::Sub =>
            {
                let scale = self.element_size(&left, left_pointee.as_ref())?;
                let right_scale = self.element_size(&right, right_pointee.as_ref())?;
                if scale != right_scale {
                    return Err(Self::error(
                        span,
                        ErrorKind::Type,
                        "the pointers point at types of different sizes",
                    ));
                }
                self.node(
                    Op::Difference {
                        left: Box::new(left),
                        right: Box::new(right),
                        scale,
                    },
                    Ty::Exact,
                    span,
                )
            }
            (Category::Bool, _) | (_, Category::Bool) => {
                let operand = if matches!(left_category, Category::Bool) {
                    &left
                } else {
                    &right
                };
                Err(Self::error(
                    operand.span,
                    ErrorKind::Type,
                    format!(
                        "`{}` is a truth value, not a number",
                        self.quote(operand.span)
                    ),
                )
                .with_hint("convert it with `as u8`"))
            }
            (category, _)
                if !matches!(
                    category,
                    Category::Integer { .. } | Category::Float(_) | Category::Pointer(_)
                ) =>
            {
                Err(self.type_error(&left, category, "is not a number"))
            }
            (_, category)
                if !matches!(
                    category,
                    Category::Integer { .. } | Category::Float(_) | Category::Pointer(_)
                ) =>
            {
                Err(self.type_error(&right, category, "is not a number"))
            }
            _ => Err(Self::error(
                span,
                ErrorKind::Type,
                format!(
                    "`{}` cannot be applied to `{}` and `{}`",
                    super::syntax::binary_operator_text(op),
                    type_name(self.scope, &left.ty),
                    type_name(self.scope, &right.ty)
                ),
            )),
        }
    }

    /// The size of the elements a pointer steps over.
    fn element_size(
        &self,
        pointer: &Bound<S>,
        target: Option<&Ty>,
    ) -> Result<u64, ExpressionError> {
        let Some(target) = target else {
            return Err(Self::error(
                pointer.span,
                ErrorKind::Type,
                format!(
                    "`{}` points at void, which has no size",
                    self.quote(pointer.span)
                ),
            )
            .with_hint("cast it to `u8*` to count bytes"));
        };
        size_of(self.scope, target)
            .filter(|&size| size > 0)
            .ok_or_else(|| {
                Self::error(
                    pointer.span,
                    ErrorKind::Type,
                    format!(
                        "`{}` points at a type without a size",
                        self.quote(pointer.span)
                    ),
                )
            })
    }

    fn float_operand(&mut self, node: Bound<S>, format: FloatFormat) -> BindResult<S> {
        let span = node.span;
        let conversion = match self.category(&node.ty) {
            Category::Float(own) if own == format => return Ok(node),
            Category::Float(_) => Conversion::FloatToFloat(format),
            _ => Conversion::IntegerToFloat(format),
        };
        self.node(
            Op::Convert {
                operand: Box::new(node),
                conversion,
            },
            Ty::Float(format),
            span,
        )
    }

    fn comparison(
        &mut self,
        op: BinaryOp,
        left: Bound<S>,
        right: Bound<S>,
        span: Span,
    ) -> BindResult<S> {
        let ordering = !matches!(op, BinaryOp::Eq | BinaryOp::Ne);
        let left = self.settle(left)?;
        let right = self.settle(right)?;
        // Text compares with a string literal, equality only.
        if matches!(left.ty, Ty::Text) || matches!(right.ty, Ty::Text) {
            let (text, other) = if matches!(left.ty, Ty::Text) {
                (left, right)
            } else {
                (right, left)
            };
            if ordering || !other.is_place() || matches!(other.ty, Ty::Text) {
                return Err(Self::error(
                    span,
                    ErrorKind::Type,
                    "a string only compares, by `==` or `!=`, with text the program holds",
                ));
            }
            return self.node(
                Op::Compare {
                    op,
                    how: Comparison::Text,
                    left: Box::new(other),
                    right: Box::new(text),
                },
                Ty::Bool,
                span,
            );
        }
        let left = self.operand(left)?;
        let right = self.operand(right)?;
        let how = match (self.category(&left.ty), self.category(&right.ty)) {
            (Category::Integer { .. }, Category::Integer { .. }) => Comparison::Integers,
            (Category::Float(_), Category::Float(_)) => Comparison::Floats,
            (Category::Float(_), Category::Integer { .. }) => Comparison::FloatInteger,
            (Category::Integer { .. }, Category::Float(_)) => Comparison::IntegerFloat,
            (Category::Pointer(_) | Category::Null, Category::Pointer(_) | Category::Null) => {
                Comparison::Addresses
            }
            (Category::Pointer(_), Category::Integer { .. }) if Self::is_zero(&right) => {
                Comparison::Addresses
            }
            (Category::Integer { .. }, Category::Pointer(_)) if Self::is_zero(&left) => {
                Comparison::Addresses
            }
            (Category::Bool, Category::Bool) if !ordering => Comparison::Bools,
            (left_category, right_category) => {
                for (node, category) in [(&left, &left_category), (&right, &right_category)] {
                    if let Some(reason) = opaque(category) {
                        return Err(Self::error(
                            node.span,
                            ErrorKind::Unsupported,
                            format!(
                                "`{}` cannot be computed with: {reason}",
                                self.quote(node.span)
                            ),
                        ));
                    }
                }
                return Err(Self::error(
                    span,
                    ErrorKind::Type,
                    format!(
                        "`{}` cannot compare `{}` with `{}`",
                        super::syntax::binary_operator_text(op),
                        type_name(self.scope, &left.ty),
                        type_name(self.scope, &right.ty)
                    ),
                ));
            }
        };
        self.node(
            Op::Compare {
                op,
                how,
                left: Box::new(left),
                right: Box::new(right),
            },
            Ty::Bool,
            span,
        )
    }

    fn is_zero(node: &Bound<S>) -> bool {
        matches!(&node.op, Op::Constant(Constant::Integer(integer)) if integer.value().is_zero())
    }

    fn conditional(
        &mut self,
        condition: NodeId,
        then: NodeId,
        otherwise: NodeId,
        span: Span,
    ) -> BindResult<S> {
        let condition = self.bind(condition)?;
        let condition = self.truth(condition)?;
        let (then, otherwise) = self.operands(then, otherwise)?;
        let then = self.settle(then)?;
        let otherwise = self.settle(otherwise)?;
        if then.is_place() && otherwise.is_place() && then.ty == otherwise.ty {
            let ty = then.ty.clone();
            return self.node(
                Op::Choose {
                    condition: Box::new(condition),
                    then: Box::new(then),
                    otherwise: Box::new(otherwise),
                    places: true,
                },
                ty,
                span,
            );
        }
        let then = self.operand(then)?;
        let otherwise = self.operand(otherwise)?;
        let (then_category, otherwise_category) =
            (self.category(&then.ty), self.category(&otherwise.ty));
        let (then, otherwise, ty) = if then.ty == otherwise.ty {
            let ty = then.ty.clone();
            (then, otherwise, ty)
        } else {
            match (then_category, otherwise_category) {
                (Category::Integer { .. }, Category::Integer { .. }) => {
                    (then, otherwise, Ty::Exact)
                }
                (
                    Category::Float(_) | Category::Integer { .. },
                    Category::Float(_) | Category::Integer { .. },
                ) => {
                    let format = [&then.ty, &otherwise.ty]
                        .into_iter()
                        .filter_map(|ty| match ty {
                            Ty::Float(format) => Some(*format),
                            _ => match self.category(ty) {
                                Category::Float(format) => Some(format),
                                _ => None,
                            },
                        })
                        .max()
                        .expect("one branch is a float");
                    let then = self.float_operand(then, format)?;
                    let otherwise = self.float_operand(otherwise, format)?;
                    (then, otherwise, Ty::Float(format))
                }
                (Category::Pointer(_), Category::Null) => {
                    let ty = then.ty.clone();
                    (then, otherwise, ty)
                }
                (Category::Null, Category::Pointer(_)) => {
                    let ty = otherwise.ty.clone();
                    (then, otherwise, ty)
                }
                _ => {
                    return Err(Self::error(
                        span,
                        ErrorKind::Type,
                        format!(
                            "the branches have different types, `{}` and `{}`",
                            type_name(self.scope, &then.ty),
                            type_name(self.scope, &otherwise.ty)
                        ),
                    ));
                }
            }
        };
        self.node(
            Op::Choose {
                condition: Box::new(condition),
                then: Box::new(then),
                otherwise: Box::new(otherwise),
                places: false,
            },
            ty,
            span,
        )
    }

    // ---- Assignment ----

    /// `target = value`, or `target op= value`, which must be the whole
    /// expression.
    fn assignment(
        &mut self,
        op: Option<BinaryOp>,
        target: NodeId,
        value: NodeId,
        span: Span,
    ) -> BindResult<S> {
        let target = self.bind(target)?;
        let target = self.settle(target)?;
        if !target.is_place() || !matches!(target.ty, Ty::Program(_)) {
            return Err(Self::error(
                target.span,
                ErrorKind::NotAnLvalue,
                format!(
                    "`{}` is a computed value, which cannot be assigned",
                    self.quote(target.span)
                ),
            ));
        }
        let category = self.category(&target.ty);
        if !matches!(
            category,
            Category::Integer { .. } | Category::Float(_) | Category::Bool | Category::Pointer(_)
        ) {
            return Err(self.type_error(
                &target,
                &category,
                "cannot be assigned; only numbers, truth values, and pointers can",
            ));
        }
        let value = self.bind_beside(value, Some(&target))?;
        let value = match op {
            None => value,
            Some(op) => self.binary_bound(op, target.clone(), value, span)?,
        };
        let value = self.operand(value)?;
        let source = self.category(&value.ty);
        let compatible = match category {
            Category::Integer { .. } => {
                matches!(
                    source,
                    Category::Integer { .. } | Category::Float(_) | Category::Bool
                )
            }
            Category::Float(_) => matches!(source, Category::Integer { .. } | Category::Float(_)),
            Category::Bool => matches!(source, Category::Bool | Category::Integer { .. }),
            _ => matches!(
                source,
                Category::Pointer(_) | Category::Null | Category::Integer { .. }
            ),
        };
        if !compatible {
            return Err(Self::error(
                value.span,
                ErrorKind::Type,
                format!(
                    "`{}` cannot be assigned to `{}`",
                    type_name(self.scope, &value.ty),
                    type_name(self.scope, &target.ty)
                ),
            ));
        }
        let (ty, value_span) = (target.ty.clone(), value.span);
        let fitted = self.node(Op::Fit(Box::new(value)), ty.clone(), value_span)?;
        self.node(
            Op::Assign {
                target: Box::new(target),
                value: Box::new(fitted),
            },
            ty,
            span,
        )
    }

    // ---- Conversions ----

    fn convert(&mut self, operand: Bound<S>, target: Ty, span: Span) -> BindResult<S> {
        let operand = self.settle(operand)?;
        if operand.ty == target {
            let mut operand = operand;
            operand.span = span;
            return Ok(operand);
        }
        let target_category = self.category(&target);
        let operand = match target_category {
            Category::Pointer(_) | Category::Integer { .. } | Category::Bool => {
                self.operand(operand)?
            }
            _ => self.value(operand)?,
        };
        let source = self.category(&operand.ty);
        let conversion = match (&source, &target_category) {
            (_, Category::Bool) => {
                return self.truth(operand).map(|mut node| {
                    node.span = span;
                    node
                });
            }
            (Category::Integer { .. }, Category::Integer { int: Some(int), .. }) => {
                Conversion::Truncate(*int)
            }
            (Category::Float(_), Category::Integer { int: Some(int), .. }) => {
                Conversion::FloatToInteger(*int)
            }
            (Category::Bool, Category::Integer { int: Some(int), .. }) => {
                Conversion::BoolToInteger(*int)
            }
            (Category::Pointer(_) | Category::Null, Category::Integer { int: Some(int), .. }) => {
                Conversion::PointerToInteger(*int)
            }
            (Category::Integer { .. }, Category::Float(format)) => {
                Conversion::IntegerToFloat(*format)
            }
            (Category::Float(_), Category::Float(format)) => Conversion::FloatToFloat(*format),
            (Category::Integer { .. }, Category::Pointer(_)) => Conversion::IntegerToPointer,
            (Category::Pointer(_) | Category::Null, Category::Pointer(_)) => {
                Conversion::PointerToPointer
            }
            (category @ Category::Opaque(_), _) => {
                return Err(self.type_error(&operand, category, "cannot be converted"));
            }
            (_, Category::Opaque(reason)) => {
                return Err(Self::error(
                    span,
                    ErrorKind::Unsupported,
                    format!(
                        "`{}` cannot be computed with: {reason}",
                        type_name(self.scope, &target)
                    ),
                ));
            }
            (Category::Record | Category::Array { .. } | Category::Slice(_), _)
            | (_, Category::Record | Category::Array { .. } | Category::Slice(_)) => {
                return Err(Self::error(
                    span,
                    ErrorKind::Type,
                    format!(
                        "`{}` cannot be converted to `{}` by value",
                        type_name(self.scope, &operand.ty),
                        type_name(self.scope, &target)
                    ),
                )
                .with_hint("convert a pointer to it instead"));
            }
            _ => {
                return Err(Self::error(
                    span,
                    ErrorKind::Type,
                    format!(
                        "`{}` cannot be converted to `{}`",
                        type_name(self.scope, &operand.ty),
                        type_name(self.scope, &target)
                    ),
                ));
            }
        };
        self.node(
            Op::Convert {
                operand: Box::new(operand),
                conversion,
            },
            target,
            span,
        )
    }

    // ---- Members, indices, lengths ----

    fn member(
        &mut self,
        base: Bound<S>,
        field: &Field,
        field_span: Span,
        arrow: bool,
        span: Span,
    ) -> BindResult<S> {
        let base = self.settle(base)?;
        let base = match self.category(&base.ty) {
            Category::Pointer(_) => self.deref(base, span)?,
            _ if arrow => {
                let category = self.category(&base.ty);
                return Err(self.type_error(&base, &category, "is not a pointer"));
            }
            _ => base,
        };
        let base = self.settle(base)?;
        let category = self.category(&base.ty);
        if !matches!(category, Category::Record) {
            let what = if matches!(category, Category::Pointer(_)) {
                "is a pointer to a pointer; `.` and `->` follow one pointer"
            } else {
                "has no members"
            };
            return Err(self.type_error(&base, &category, what));
        }
        let Ty::Program(from) = base.ty else {
            unreachable!("records are program types")
        };
        let names = match field {
            Field::Named(name) => vec![name.clone()],
            Field::Index(index) => vec![format!("__{index}"), index.to_string()],
        };
        let mut planned = Err(Refusal::new(ErrorKind::Type, String::new()));
        for name in &names {
            planned = self.scope.plan(from, StepKind::Member(name));
            if planned.is_ok() {
                break;
            }
        }
        let planned = planned.map_err(|refusal| Self::refused(field_span, refusal))?;
        let ty = planned.result.map_or_else(
            || {
                Err(Self::error(
                    field_span,
                    ErrorKind::Unsupported,
                    format!(
                        "`{}` has a type the debugger cannot compute with",
                        self.quote(span)
                    ),
                ))
            },
            |ty| Ok(Ty::Program(ty)),
        )?;
        self.node(
            Op::Step {
                base: Box::new(base),
                step: planned.step,
                indices: Vec::new(),
                follows: false,
            },
            ty,
            span,
        )
    }

    /// `a[i][j]...`: an array takes as many consecutive indices as it has
    /// dimensions.
    fn index(&mut self, id: NodeId) -> BindResult<S> {
        let mut chain = Vec::new();
        let mut current = id;
        while let NodeKind::Index { base, index } = self.tree().kind(current) {
            chain.push((*index, self.tree().span(current)));
            current = *base;
        }
        chain.reverse();
        let mut node = self.bind(current)?;
        let mut pending = chain.as_slice();
        while let [(first, span), ..] = pending {
            node = self.settle(node)?;
            let category = self.category(&node.ty);
            match category {
                ref category @ (Category::Array { .. } | Category::Slice(_)) if node.is_place() => {
                    let Ty::Program(from) = node.ty else {
                        unreachable!("arrays are program types")
                    };
                    let planned = self
                        .scope
                        .plan(
                            from,
                            StepKind::Index {
                                available: pending.len(),
                            },
                        )
                        .map_err(|refusal| Self::refused(*span, refusal))?;
                    let consumed = planned.consumed.clamp(1, pending.len());
                    let (taken, rest) = pending.split_at(consumed);
                    let mut indices = Vec::new();
                    for (index, _) in taken {
                        let index = self.bind(*index)?;
                        indices.push(self.integer_value(index)?);
                    }
                    let span = taken.last().map_or(*span, |(_, span)| *span);
                    let ty = planned.result.map_or_else(
                        || {
                            Err(Self::error(
                                span,
                                ErrorKind::Unsupported,
                                "the element has a type the debugger cannot compute with",
                            ))
                        },
                        |ty| Ok(Ty::Program(ty)),
                    )?;
                    node = self.node(
                        Op::Step {
                            base: Box::new(node),
                            step: planned.step,
                            indices,
                            follows: matches!(category, Category::Slice(_)),
                        },
                        ty,
                        span,
                    )?;
                    pending = rest;
                }
                Category::Pointer(target) => {
                    let pointer = self.value(node)?;
                    let scale = self.element_size(&pointer, target.as_ref())?;
                    let index = self.bind(*first)?;
                    let index = self.integer_value(index)?;
                    let ty = pointer.ty.clone();
                    let moved = self.node(
                        Op::Offset {
                            pointer: Box::new(pointer),
                            count: Box::new(index),
                            scale,
                            backward: false,
                        },
                        ty,
                        *span,
                    )?;
                    let target = target.expect("a sized pointee");
                    node = self.deref_value(moved, &target, *span)?;
                    pending = &pending[1..];
                }
                category => return Err(self.type_error(&node, &category, "cannot be indexed")),
            }
        }
        Ok(node)
    }

    fn length(&mut self, operand: Bound<S>, span: Span) -> BindResult<S> {
        let operand = self.settle(operand)?;
        let category = self.category(&operand.ty);
        let constant =
            |count: u64| Constant::Integer(Integer::Exact(Exact::from(u128::from(count))));
        match category {
            Category::Array { dimensions, .. } => {
                let count = dimensions.first().map_or(0, |dimension| dimension.count);
                self.node(Op::Constant(constant(count)), Ty::Exact, span)
            }
            Category::Slice(_) if operand.is_place() => self.node(
                Op::Length {
                    operand: Box::new(operand),
                    how: Length::Slice,
                },
                Ty::Exact,
                span,
            ),
            Category::Pointer(Some(ref pointee))
                if operand.is_place() && is_character(self.scope, pointee) =>
            {
                self.node(
                    Op::Length {
                        operand: Box::new(operand),
                        how: Length::Text,
                    },
                    Ty::Exact,
                    span,
                )
            }
            Category::Record if operand.is_place() => self.node(
                Op::Length {
                    operand: Box::new(operand),
                    how: Length::Text,
                },
                Ty::Exact,
                span,
            ),
            category => Err(self.type_error(&operand, &category, "has no length")),
        }
    }
}

/// Helpers the binder shares with the interpreter.
pub(super) fn float_from(value: crate::FloatValue) -> Float {
    Float::from_value(value)
}
