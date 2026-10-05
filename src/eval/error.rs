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
}

impl ErrorKind {
    /// The kind's stable name, as the reference's examples write it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Syntax => "syntax",
            Self::Limit => "limit",
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
