//! Reading an expression's text, which needs no program.
//!
//! Most text has one reading. The exception is a parenthesized name before
//! `-`, `*`, or `&`: `(n) - 1` subtracts when `n` is a variable, and
//! `(T) - 1` casts `-1` when `T` is a type, and the two readings group the
//! rest of the expression differently. Such spots are found from the tokens
//! alone, and the text is read once for every combination of their
//! readings, so that binding, which knows what each name is, picks the tree
//! that matches.

pub mod ast;
mod lexer;
mod parser;
mod print;

use std::fmt;
use std::sync::Arc;

pub use ast::Tree;
use ast::{BinaryOp, Path};

/// A path as it is written.
pub fn print_path(path: &Path) -> String {
    print::path_text(path)
}

/// A binary operator as it is written.
pub const fn binary_operator_text(op: BinaryOp) -> &'static str {
    parser::binary_text(op)
}

use super::error::ExpressionError;

/// A range of bytes in an expression's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    /// The span of `start..end`. Text is at most a few kilobytes, so its
    /// offsets fit `u32`.
    #[must_use]
    pub fn new(start: usize, end: usize) -> Self {
        let offset = |value: usize| u32::try_from(value).unwrap_or(u32::MAX);
        Self {
            start: offset(start),
            end: offset(end),
        }
    }

    /// The span covering both.
    #[must_use]
    pub fn to(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    /// The text this span covers.
    #[must_use]
    pub fn text(self, text: &str) -> &str {
        text.get(self.start as usize..self.end as usize)
            .unwrap_or_default()
    }
}

/// The most parenthesized names before `-`, `*`, or `&` one expression may
/// hold; each doubles the readings.
pub const MAX_AMBIGUITIES: usize = 4;

/// A parenthesized name whose reading binding decides: as a value, the
/// parentheses group it; as a type, they cast what follows.
#[derive(Debug, Clone)]
pub struct Ambiguity {
    pub name: Path,
    /// The name and its parentheses.
    pub span: Span,
}

/// A parsed expression. Cloning it is cheap.
#[derive(Debug, Clone)]
pub struct Expression {
    parsed: Arc<Parsed>,
}

#[derive(Debug)]
struct Parsed {
    text: String,
    ambiguities: Vec<Ambiguity>,
    /// One reading per combination: bit `i` of the index is set when
    /// ambiguity `i` reads as a cast.
    readings: Vec<Result<Tree, ExpressionError>>,
}

impl Expression {
    /// Parses `text`. It fails only when no reading of the text is an
    /// expression.
    pub fn parse(text: &str) -> Result<Self, ExpressionError> {
        let (ambiguities, readings) = parser::parse(text)?;
        Ok(Self {
            parsed: Arc::new(Parsed {
                text: text.to_owned(),
                ambiguities,
                readings,
            }),
        })
    }

    /// The text as written.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.parsed.text
    }

    #[must_use]
    pub fn ambiguities(&self) -> &[Ambiguity] {
        &self.parsed.ambiguities
    }

    /// The reading where ambiguity `i` is a cast exactly when bit `i` of
    /// `casts` is set.
    pub fn reading(&self, casts: usize) -> Result<&Tree, &ExpressionError> {
        self.parsed.readings[casts].as_ref()
    }

    /// The only reading, for text without ambiguities.
    #[must_use]
    pub fn tree(&self) -> Option<&Tree> {
        match self.parsed.readings.as_slice() {
            [Ok(tree)] => Some(tree),
            _ => None,
        }
    }

    /// The text of an assignment's target, when the expression assigns.
    #[must_use]
    pub fn assignment_target(&self) -> Option<&str> {
        let tree = self.tree()?;
        match tree.kind(tree.root()) {
            ast::NodeKind::Assign { target, .. } => Some(tree.span(*target).text(self.text())),
            _ => None,
        }
    }

    /// Whether both parse the same, whatever their spacing.
    #[must_use]
    pub fn same_shape(&self, other: &Self) -> bool {
        self.parsed.ambiguities.len() == other.parsed.ambiguities.len()
            && self
                .parsed
                .ambiguities
                .iter()
                .zip(&other.parsed.ambiguities)
                .all(|(left, right)| left.name == right.name)
            && self
                .parsed
                .readings
                .iter()
                .zip(&other.parsed.readings)
                .all(|pair| match pair {
                    (Ok(left), Ok(right)) => left.same_shape(right),
                    (Err(left), Err(right)) => left.kind == right.kind,
                    _ => false,
                })
    }
}

/// Two expressions are equal when their text is: one text has one set of
/// readings.
impl PartialEq for Expression {
    fn eq(&self, other: &Self) -> bool {
        self.parsed.text == other.parsed.text
    }
}

impl Eq for Expression {}

/// The expression in normal form: one space around binary operators and
/// only the parentheses its meaning needs, which parses back to the same
/// tree. Text with ambiguities prints as written, since parentheses one
/// reading does not need may matter to another.
impl fmt::Display for Expression {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.tree() {
            Some(tree) => formatter.write_str(&print::print(tree)),
            None => formatter.write_str(self.parsed.text.trim()),
        }
    }
}

/// Checks what must hold of any text: parsing never panics, an error
/// points inside the text, every span of every reading is a balanced slice
/// nested in its parent's, and the normal form reads back the same.
#[cfg(any(test, feature = "fuzzing"))]
pub fn check_invariants(text: &str) -> Result<(), String> {
    let expression = match Expression::parse(text) {
        Ok(expression) => expression,
        Err(error)
            if error.span.start <= error.span.end
                && error.span.end as usize <= text.len().max(1) =>
        {
            return Ok(());
        }
        Err(error) => return Err(format!("{error:?} points outside `{text}`")),
    };
    for casts in 0..1 << expression.ambiguities().len() {
        if let Ok(tree) = expression.reading(casts) {
            tree.check_spans(text)?;
        }
    }
    let printed = expression.to_string();
    let reparsed = match Expression::parse(&printed) {
        Ok(reparsed) => reparsed,
        // Spaces and parentheses can take text near a limit past it.
        Err(error) if error.kind == super::error::ErrorKind::Limit => return Ok(()),
        Err(error) => return Err(format!("`{printed}` from `{text}`: {error}")),
    };
    if !reparsed.same_shape(&expression) {
        return Err(format!(
            "`{text}` printed as `{printed}`, which reads differently"
        ));
    }
    if reparsed.to_string() != printed {
        return Err(format!("`{printed}` printed again as `{reparsed}`"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
