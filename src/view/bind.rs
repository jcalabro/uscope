//! Binding a view against one concrete type, before anything runs: every
//! member path resolves, every type resolves through the type index, and
//! every expression type-checks. An `or` alternative that does not bind is
//! skipped; a view any part of which does not bind is rejected with its
//! reason, and the next candidate is tried.

use std::sync::Arc;

use crate::eval::bind::{Mode, bind as bind_expression, bind_condition, bind_value};
use crate::eval::error::ErrorKind;
use crate::eval::ir::Program;
use crate::eval::number::Exact;
use crate::eval::syntax::Expression;
use crate::eval::syntax::ast::{BinaryOp, NodeKind};
use crate::eval::target::{
    Lookup, Planned, Refusal, Register, Scope, StepKind, TypeLookup, TypeQuery,
};
use crate::eval::types::{Category, Ty, TypeSource, category, representation};
use crate::{BaseTypeEncoding, TypeArgument, TypeInfo, TypeKind, TypeReference};

use super::pattern::{Captured, Captures};
use super::syntax::{Count, Expr, Piece, Shape, Statement, TypeExpr, View};

/// Something a view's expression names, which its machine reaches at a
/// stop.
#[derive(Debug, Clone)]
pub enum ViewObject<St> {
    /// `self`, the value presented.
    This,
    /// A member of `self`, by its planned step.
    Member(St),
    /// A `let`, by position.
    Let(usize),
    /// A generator's variable, by nesting depth.
    Variable(usize),
}

/// A program bound in a view's scope.
pub type ViewProgram<St> = Program<ViewObject<St>, St>;

/// A `let`, computed at most once per presentation.
#[derive(Debug, Clone)]
pub struct BoundLet<St> {
    pub program: ViewProgram<St>,
}

/// A `check`, with its sides when it is a comparison, for saying why it
/// failed.
#[derive(Debug, Clone)]
pub struct BoundCheck<St> {
    pub text: String,
    pub program: ViewProgram<St>,
    pub sides: Option<[(String, ViewProgram<St>); 2]>,
}

/// A `field`.
#[derive(Debug, Clone)]
pub struct BoundField<St> {
    pub name: Arc<str>,
    pub program: ViewProgram<St>,
}

/// A piece of a `summary`.
#[derive(Debug, Clone)]
pub enum BoundPiece<St> {
    Literal(String),
    Hole(ViewProgram<St>),
}

/// Where a `text` shape's bytes are.
#[derive(Debug, Clone)]
pub enum TextSource<St> {
    /// A pointer to one-byte characters.
    Pointer(ViewProgram<St>),
    /// An array or slice of one-byte elements, with the step to its first.
    Elements { program: ViewProgram<St>, first: St },
}

/// A shape, bound.
#[derive(Debug, Clone)]
pub enum BoundShape<St> {
    Text {
        source: TextSource<St>,
        length: Option<ViewProgram<St>>,
    },
    Value(ViewProgram<St>),
    Empty(Arc<str>),
    /// `count` is `None` when the view leaves it to the range.
    Sequence {
        count: Option<ViewProgram<St>>,
        length: ViewProgram<St>,
        element: ViewProgram<St>,
    },
    If {
        condition: ViewProgram<St>,
        then: Box<Self>,
        otherwise: Box<Self>,
    },
}

impl<St> BoundShape<St> {
    /// Whether any branch presents text.
    pub fn has_text(&self) -> bool {
        match self {
            Self::Text { .. } => true,
            Self::If {
                then, otherwise, ..
            } => then.has_text() || otherwise.has_text(),
            _ => false,
        }
    }

    /// Whether any branch presents a sequence, whose elements are children.
    pub fn has_elements(&self) -> bool {
        match self {
            Self::Sequence { .. } => true,
            Self::If {
                then, otherwise, ..
            } => then.has_elements() || otherwise.has_elements(),
            _ => false,
        }
    }
}

/// A view bound against one concrete type.
#[derive(Debug, Clone)]
pub struct BoundView<St> {
    pub view: Arc<View>,
    pub lets: Vec<BoundLet<St>>,
    pub checks: Vec<BoundCheck<St>>,
    pub fields: Vec<BoundField<St>>,
    pub summary: Option<Vec<BoundPiece<St>>>,
    pub shape: BoundShape<St>,
}

/// Why a view does not bind: the part that failed, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub line: u32,
    /// The part of the view that failed, as written.
    pub part: String,
    pub reason: String,
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "line {}: `{}`: {}",
            self.line, self.part, self.reason
        )
    }
}

/// The scope a view's expressions bind in: its generators' variables, its
/// `let`s, the values its pattern captured, `self`, and `self`'s members.
/// Types are its captures and `type`s, then the program's. Nothing the
/// frame names is visible, so a view means the same at every stop.
pub struct ViewScope<'a, S: Scope> {
    base: &'a S,
    self_type: TypeReference,
    captures: &'a Captures,
    types: Vec<(String, TypeReference)>,
    lets: Vec<(String, Ty, bool)>,
    variables: Vec<String>,
}

impl<'a, S: Scope> ViewScope<'a, S> {
    pub const fn new(base: &'a S, self_type: TypeReference, captures: &'a Captures) -> Self {
        Self {
            base,
            self_type,
            captures,
            types: Vec::new(),
            lets: Vec::new(),
            variables: Vec::new(),
        }
    }

    fn self_name(&self) -> String {
        self.base
            .type_info(self.self_type)
            .map_or_else(|| "self".to_owned(), |info| info.name.to_string())
    }
}

impl<S: Scope> TypeSource for ViewScope<'_, S> {
    fn type_info(&self, ty: TypeReference) -> Option<TypeInfo> {
        self.base.type_info(ty)
    }

    fn pointer_size(&self) -> u8 {
        self.base.pointer_size()
    }

    fn byte_order(&self) -> crate::ByteOrder {
        self.base.byte_order()
    }
}

impl<S: Scope> Scope for ViewScope<'_, S> {
    type Object = ViewObject<S::Step>;
    type Step = S::Step;

    fn lookup(&self, name: &str, outermost: bool) -> Result<Lookup<Self::Object>, Refusal> {
        if outermost {
            return Ok(Lookup::NotFound);
        }
        if let Some(depth) = self.variables.iter().rposition(|variable| variable == name) {
            return Ok(Lookup::Bound {
                object: ViewObject::Variable(depth),
                ty: Ty::Exact,
            });
        }
        if let Some(index) = self
            .lets
            .iter()
            .rposition(|(let_name, ..)| let_name == name)
        {
            let (_, ty, place) = &self.lets[index];
            return Ok(match (place, ty) {
                (true, Ty::Program(reference)) => Lookup::Object {
                    object: ViewObject::Let(index),
                    ty: Ok(*reference),
                },
                _ => Lookup::Bound {
                    object: ViewObject::Let(index),
                    ty: ty.clone(),
                },
            });
        }
        match self.captures.iter().find(|(captured, _)| captured == name) {
            Some((_, Captured::Value(value))) => {
                return Ok(Lookup::Constant(match value {
                    crate::IntegerValue::Signed(value) => Exact::from(*value),
                    crate::IntegerValue::Unsigned(value) => Exact::from(*value),
                }));
            }
            Some((_, Captured::Type(_))) => return Ok(Lookup::NotFound),
            None => {}
        }
        if name == "self" {
            return Ok(Lookup::Object {
                object: ViewObject::This,
                ty: Ok(self.self_type),
            });
        }
        match self.base.plan(self.self_type, StepKind::Member(name)) {
            Ok(Planned { step, result, .. }) => Ok(Lookup::Object {
                object: ViewObject::Member(step),
                ty: result.ok_or_else(|| {
                    Arc::from(format!(
                        "the member `{name}` has a type the debugger cannot compute with"
                    ))
                }),
            }),
            Err(refusal) if refusal.kind == ErrorKind::Type => Ok(Lookup::NotFound),
            Err(refusal) => Err(refusal),
        }
    }

    fn lookup_type(&self, query: &TypeQuery) -> TypeLookup {
        if query.tag.is_none() {
            if let Some((_, reference)) = self
                .types
                .iter()
                .rev()
                .find(|(name, _)| *name == query.name)
            {
                return TypeLookup::Found(*reference);
            }
            if let Some((_, Captured::Type(reference))) = self
                .captures
                .iter()
                .find(|(captured, _)| *captured == query.name)
            {
                return TypeLookup::Found(*reference);
            }
        }
        self.base.lookup_type(query)
    }

    fn plan(
        &self,
        from: TypeReference,
        step: StepKind<'_>,
    ) -> Result<Planned<Self::Step>, Refusal> {
        self.base.plan(from, step)
    }

    fn register(&self, _name: &str) -> Option<Register> {
        None
    }
}

/// Binds `view` against `self_type`, whose identity its pattern matched,
/// capturing `captures`.
pub fn bind<S: Scope>(
    view: &Arc<View>,
    self_type: TypeReference,
    captures: &Captures,
    base: &S,
) -> Result<BoundView<S::Step>, Rejection> {
    let mut scope = ViewScope::new(base, self_type, captures);
    let mut lets = Vec::new();
    // `let`s and `type`s, in order: each sees those before it.
    for statement in &view.statements {
        match statement {
            Statement::Let {
                name,
                alternatives,
                line,
            } => {
                let mut reasons = Vec::new();
                let mut bound = None;
                for alternative in alternatives {
                    match bind_value(&alternative.expression, &scope) {
                        Ok(program) => {
                            bound = Some(program);
                            break;
                        }
                        Err(error) => reasons.push(explain(&scope, alternative, &error)),
                    }
                }
                let Some(program) = bound else {
                    return Err(Rejection {
                        line: *line,
                        part: format!("let {name}"),
                        reason: reasons.join("; or "),
                    });
                };
                let ty = program.result().clone();
                let place = program.is_place() && matches!(ty, Ty::Program(_));
                scope.lets.push((name.clone(), ty, place));
                lets.push(BoundLet { program });
            }
            Statement::Type {
                name,
                alternatives,
                line,
            } => {
                let mut reasons = Vec::new();
                let mut found = None;
                for alternative in alternatives {
                    match resolve_type(alternative, &scope) {
                        Ok(reference) => {
                            found = Some(reference);
                            break;
                        }
                        Err(reason) => reasons.push(reason),
                    }
                }
                let Some(reference) = found else {
                    return Err(Rejection {
                        line: *line,
                        part: format!("type {name}"),
                        reason: reasons.join("; or "),
                    });
                };
                scope.types.push((name.clone(), reference));
            }
            _ => {}
        }
    }
    let mut checks = Vec::new();
    let mut fields = Vec::new();
    let mut summary = None;
    let mut shape = None;
    for statement in &view.statements {
        match statement {
            Statement::Check(expr) => {
                for conjunct in conjuncts(expr) {
                    checks.push(bind_check(&conjunct, &scope)?);
                }
            }
            Statement::Field { name, value } => fields.push(BoundField {
                name: name.as_str().into(),
                program: bind_part(value, &scope, Mode::Read)?,
            }),
            Statement::Summary(pieces) => {
                let mut bound = Vec::new();
                for piece in pieces {
                    bound.push(match piece {
                        Piece::Literal(text) => BoundPiece::Literal(text.clone()),
                        Piece::Hole(expr) => BoundPiece::Hole(bind_part(expr, &scope, Mode::Read)?),
                    });
                }
                summary = Some(bound);
            }
            Statement::Show(shown) => shape = Some(bind_shape(shown, &mut scope)?),
            Statement::Let { .. } | Statement::Type { .. } => {}
        }
    }
    Ok(BoundView {
        view: Arc::clone(view),
        lets,
        checks,
        fields,
        summary,
        shape: shape.expect("the parser requires one `show`"),
    })
}

/// An error binding `expr`, said in terms of the view.
fn explain<S: Scope>(
    scope: &ViewScope<'_, S>,
    expr: &Expr,
    error: &crate::ExpressionError,
) -> String {
    let at = error.span.text(expr.expression.text());
    if error.kind == ErrorKind::UnknownName {
        return format!(
            "`{at}` is neither a member of `{}` nor a name the view declares",
            scope.self_name()
        );
    }
    if at.is_empty() || at == expr.text() {
        error.message.clone()
    } else {
        format!("`{at}`: {}", error.message)
    }
}

fn rejection<S: Scope>(
    scope: &ViewScope<'_, S>,
    expr: &Expr,
    error: &crate::ExpressionError,
) -> Rejection {
    Rejection {
        line: expr.line,
        part: expr.text().to_owned(),
        reason: explain(scope, expr, error),
    }
}

fn bind_part<S: Scope>(
    expr: &Expr,
    scope: &ViewScope<'_, S>,
    mode: Mode,
) -> Result<ViewProgram<S::Step>, Rejection> {
    bind_expression(&expr.expression, scope, mode).map_err(|error| rejection(scope, expr, &error))
}

/// Binds an expression whose value must be an integer.
fn bind_integer<S: Scope>(
    expr: &Expr,
    scope: &ViewScope<'_, S>,
) -> Result<ViewProgram<S::Step>, Rejection> {
    let program =
        bind_value(&expr.expression, scope).map_err(|error| rejection(scope, expr, &error))?;
    if matches!(category(scope, program.result()), Category::Integer { .. }) {
        Ok(program)
    } else {
        Err(Rejection {
            line: expr.line,
            part: expr.text().to_owned(),
            reason: "is not an integer".to_owned(),
        })
    }
}

/// A check's conjuncts, each its own check, so a failure names the one
/// that does not hold.
fn conjuncts(expr: &Expr) -> Vec<Expr> {
    fn split(
        tree: &crate::eval::syntax::Tree,
        id: crate::eval::syntax::ast::NodeId,
        text: &str,
        out: &mut Vec<String>,
    ) {
        match tree.kind(id) {
            NodeKind::Binary {
                op: BinaryOp::And,
                left,
                right,
            } => {
                split(tree, *left, text, out);
                split(tree, *right, text, out);
            }
            _ => out.push(tree.span(id).text(text).trim().to_owned()),
        }
    }
    let Some(tree) = expr.expression.tree() else {
        return vec![expr.clone()];
    };
    let mut texts = Vec::new();
    split(tree, tree.root(), expr.expression.text(), &mut texts);
    let parsed = texts
        .iter()
        .map(|text| Expression::parse_view(text))
        .collect::<Result<Vec<_>, _>>();
    match parsed {
        Ok(expressions) if expressions.len() > 1 => expressions
            .into_iter()
            .map(|expression| Expr {
                expression,
                line: expr.line,
            })
            .collect(),
        _ => vec![expr.clone()],
    }
}

fn bind_check<S: Scope>(
    expr: &Expr,
    scope: &ViewScope<'_, S>,
) -> Result<BoundCheck<S::Step>, Rejection> {
    let program =
        bind_condition(&expr.expression, scope).map_err(|error| rejection(scope, expr, &error))?;
    // A comparison's sides say why it failed.
    let sides = expr.expression.tree().and_then(|tree| {
        let NodeKind::Binary { op, left, right } = tree.kind(tree.root()) else {
            return None;
        };
        if !matches!(
            op,
            BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
        ) {
            return None;
        }
        let side = |id| {
            let text = tree.span(id).text(expr.expression.text()).trim().to_owned();
            let expression = Expression::parse_view(&text).ok()?;
            let program = bind_value(&expression, scope).ok()?;
            Some((text, program))
        };
        Some([side(*left)?, side(*right)?])
    });
    Ok(BoundCheck {
        text: expr.text().to_owned(),
        program,
        sides,
    })
}

fn bind_shape<S: Scope>(
    shape: &Shape,
    scope: &mut ViewScope<'_, S>,
) -> Result<BoundShape<S::Step>, Rejection> {
    Ok(match shape {
        Shape::Text { pointer, length } => BoundShape::Text {
            source: bind_text_source(pointer, scope)?,
            length: length
                .as_ref()
                .map(|length| bind_integer(length, scope))
                .transpose()?,
        },
        Shape::Value(value) => BoundShape::Value(bind_part(value, scope, Mode::Read)?),
        Shape::Empty(text) => BoundShape::Empty(text.as_str().into()),
        Shape::Sequence {
            count,
            variable,
            length,
            element,
        } => {
            let count = match count {
                Count::Known(count) => Some(bind_integer(count, scope)?),
                Count::Unknown => None,
            };
            let length = bind_integer(length, scope)?;
            scope.variables.push(variable.clone());
            let element = bind_part(element, scope, Mode::Read);
            scope.variables.pop();
            BoundShape::Sequence {
                count,
                length,
                element: element?,
            }
        }
        Shape::If {
            condition,
            then,
            otherwise,
        } => BoundShape::If {
            condition: bind_condition(&condition.expression, scope)
                .map_err(|error| rejection(scope, condition, &error))?,
            then: Box::new(bind_shape(then, scope)?),
            otherwise: Box::new(bind_shape(otherwise, scope)?),
        },
    })
}

/// Whether a type's values are one byte of text: a character or a byte.
fn is_text_unit<S: Scope>(scope: &ViewScope<'_, S>, ty: &Ty) -> bool {
    match ty {
        Ty::Int(int) => int.width() == 8,
        Ty::Program(reference) => matches!(
            representation(scope, *reference),
            Ok((_, TypeInfo {
                kind: TypeKind::Base(base),
                ..
            })) if base.byte_size == 1 && !matches!(
                base.encoding,
                BaseTypeEncoding::Boolean | BaseTypeEncoding::Floating
            )
        ),
        _ => false,
    }
}

fn bind_text_source<S: Scope>(
    pointer: &Expr,
    scope: &ViewScope<'_, S>,
) -> Result<TextSource<S::Step>, Rejection> {
    let program = bind_value(&pointer.expression, scope)
        .map_err(|error| rejection(scope, pointer, &error))?;
    let refuse = || Rejection {
        line: pointer.line,
        part: pointer.text().to_owned(),
        reason: "`text` takes a pointer to one-byte characters, or an array or slice of them"
            .to_owned(),
    };
    match category(scope, program.result()) {
        Category::Pointer(Some(target)) if is_text_unit(scope, &target) => {
            Ok(TextSource::Pointer(program))
        }
        Category::Array { element, .. } | Category::Slice(element)
            if program.is_place() && is_text_unit(scope, &Ty::Program(element)) =>
        {
            let Ty::Program(from) = program.result() else {
                return Err(refuse());
            };
            let planned = scope
                .plan(*from, StepKind::Index { available: 1 })
                .map_err(|refusal| Rejection {
                    line: pointer.line,
                    part: pointer.text().to_owned(),
                    reason: refusal.message,
                })?;
            if planned.consumed != 1 {
                return Err(refuse());
            }
            Ok(TextSource::Elements {
                program,
                first: planned.step,
            })
        }
        _ => Err(refuse()),
    }
}

/// The type a `type` statement's alternative names.
fn resolve_type<S: Scope>(
    expr: &TypeExpr,
    scope: &ViewScope<'_, S>,
) -> Result<TypeReference, String> {
    match expr {
        TypeExpr::Named { name, pointers } => {
            if *pointers != 0 {
                return Err(format!(
                    "`{name}*`: a `type` names a type; write the pointer where it is used"
                ));
            }
            match scope.lookup_type(&TypeQuery {
                name: name.clone(),
                tag: None,
            }) {
                TypeLookup::Found(reference) => Ok(reference),
                TypeLookup::Ambiguous(candidates) => Err(format!(
                    "`{name}` names several types: {}",
                    candidates.join(", ")
                )),
                TypeLookup::NotFound => Err(format!("no type is named `{name}`")),
            }
        }
        TypeExpr::TypeOf(expr) => {
            let program = bind_expression(&expr.expression, scope, Mode::Read)
                .map_err(|error| format!("`{}`: {}", expr.text(), explain(scope, expr, &error)))?;
            match program.result() {
                Ty::Program(reference) => Ok(*reference),
                _ => Err(format!("`{}` has no type of the program's", expr.text())),
            }
        }
        TypeExpr::Arg { of, index } => {
            let of = resolve_type(of, scope)?;
            let info = representation(scope, of)
                .map_err(|reason| reason.to_string())?
                .1;
            let argument = info
                .identity
                .as_ref()
                .and_then(|identity| identity.arguments.get(*index as usize).cloned());
            match argument {
                Some(TypeArgument::Type(reference)) => Ok(reference),
                Some(_) => Err(format!("argument {index} of `{}` is not a type", info.name)),
                None => Err(format!("`{}` has no argument {index}", info.name)),
            }
        }
    }
}
