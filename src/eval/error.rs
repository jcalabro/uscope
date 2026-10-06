//! Typed expression errors, each pointing at the text it is about.

use std::fmt;

use super::syntax::Span;

/// What kind of mistake an expression has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The text is not an expression.
    Syntax,
    /// The expression exceeds a size, depth, or ambiguity limit.
    Limit,
    /// A name means nothing in the scope.
    UnknownName,
    /// A name means several things; the message says how to choose.
    AmbiguousName,
    /// An operator does not apply to its operand's type.
    Type,
    /// An index is outside an array's bounds.
    Bounds,
    /// A map holds no entry for a key.
    MissingKey,
    /// Arithmetic has no result: a division by zero, a result beyond 128
    /// bits, a shift out of range, or a value that does not fit.
    Arithmetic,
    /// An operand that must be in memory is not.
    NotAnLvalue,
    /// The expression assigns where assigning is not allowed.
    Mode,
    /// An assignment's value does not fit its target.
    Assignment,
    /// An operand has a type the debugger cannot compute with.
    Unsupported,
}

impl ErrorKind {
    /// The kind's stable name, as the reference's examples write it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Syntax => "syntax",
            Self::Limit => "limit",
            Self::UnknownName => "unknown-name",
            Self::AmbiguousName => "ambiguous-name",
            Self::Type => "type",
            Self::Bounds => "bounds",
            Self::MissingKey => "missing-key",
            Self::Arithmetic => "arithmetic",
            Self::NotAnLvalue => "not-an-lvalue",
            Self::Mode => "mode",
            Self::Assignment => "assignment",
            Self::Unsupported => "unsupported",
        }
    }
}

/// Why an expression has no value, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpressionError {
    pub kind: ErrorKind,
    /// The bytes of the expression's text the error is about.
    pub span: Span,
    pub message: String,
    /// A suggestion that rewrites the expression, when one is known.
    pub hint: Option<String>,
}

impl ExpressionError {
    pub fn new(kind: ErrorKind, span: Span, message: impl Into<String>) -> Self {
        Self {
            kind,
            span,
            message: message.into(),
            hint: None,
        }
    }

    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn syntax(span: Span, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Syntax, span, message)
    }
}

impl fmt::Display for ExpressionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)?;
        if let Some(hint) = &self.hint {
            write!(formatter, " ({hint})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ExpressionError {}
