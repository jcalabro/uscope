//! Reading view files (`docs/views.md`), which needs no program.
//!
//! A file is statements in views. Each expression inside is an ordinary
//! expression in the view dialect, so the parser here finds where each one
//! ends and hands its text to the expression parser. An expression ends at
//! the end of its statement, at a `,` or `)` that closes what holds it, at
//! a `{` or `}`, or before one of the words `or`, `for`, `if`, `else`, and
//! the arrow `=>`. A statement ends at a line that begins another statement
//! or closes the view, so a long expression may continue on the next line.
//!
//! An error in one view skips that view; the views after it are still read.

use std::sync::Arc;

use crate::IntegerValue;
use crate::eval::syntax::Expression;

/// The view language's version, which a file names in its first line.
pub const VERSION: u32 = 1;

/// The largest view file read, in bytes.
pub const MAX_FILE_BYTES: usize = 256 * 1024;

/// The most views one file may hold.
pub const MAX_VIEWS: usize = 1024;

/// The most statements one view may hold.
const MAX_STATEMENTS: usize = 256;

/// How deeply shapes may nest in `if` branches.
const MAX_SHAPE_DEPTH: usize = 16;

/// How deeply type patterns may nest in arguments.
const MAX_PATTERN_DEPTH: usize = 16;

/// Words that end an expression, so a member with one of these names must
/// be written in backticks.
const STOP_WORDS: [&str; 4] = ["or", "for", "if", "else"];

/// Words that begin a statement.
const STATEMENTS: [&str; 8] = [
    "let", "type", "check", "summary", "field", "show", "if", "view",
];

/// The languages a view applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    C,
    Cpp,
    Rust,
    Go,
    Zig,
    /// Every language.
    Any,
}

impl Language {
    fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "c" => Self::C,
            "c++" => Self::Cpp,
            "rust" => Self::Rust,
            "go" => Self::Go,
            "zig" => Self::Zig,
            "any" => Self::Any,
            _ => return None,
        })
    }
}

/// A problem in a view file, at its line and column, counted from one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub source: Arc<str>,
    pub line: u32,
    pub column: u32,
    pub message: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}:{}:{}: {}",
            self.source, self.line, self.column, self.message
        )
    }
}

/// A parsed view file: the views it holds, and the problems that kept
/// others out.
#[derive(Debug, Clone)]
pub struct File {
    pub views: Vec<Arc<View>>,
    pub errors: Vec<Error>,
}

/// One view: which types it matches and how it presents them.
#[derive(Debug, Clone)]
pub struct View {
    /// The file it came from.
    pub source: Arc<str>,
    /// The line of its `view` keyword.
    pub line: u32,
    pub language: Language,
    pub pattern: Pattern,
    /// The language and pattern as written, to name the view by.
    pub header: Arc<str>,
    pub statements: Vec<Statement>,
}

/// An expression and the line it was written on.
#[derive(Debug, Clone)]
pub struct Expr {
    pub expression: Expression,
    pub line: u32,
}

impl Expr {
    /// The expression as written.
    #[must_use]
    pub fn text(&self) -> &str {
        self.expression.text().trim()
    }
}

/// A type a `type` statement names.
#[derive(Debug, Clone)]
pub enum TypeExpr {
    /// A type by name: a captured argument, a `type` statement's name, or
    /// a program type, with pointers.
    Named { name: String, pointers: u8 },
    /// The type of an expression's value.
    TypeOf(Expr),
    /// One argument of a type, by position.
    Arg { of: Box<Self>, index: u32 },
}

/// One `{expression}` or literal piece of a `summary`.
#[derive(Debug, Clone)]
pub enum Piece {
    Literal(String),
    Hole(Expr),
}

/// A statement of a view's body.
#[derive(Debug, Clone)]
pub enum Statement {
    /// `let NAME = EXPR or EXPR…`: the first alternative that binds.
    Let {
        name: String,
        alternatives: Vec<Expr>,
        line: u32,
    },
    /// `type NAME = TYPE or TYPE…`: the first alternative that resolves.
    Type {
        name: String,
        alternatives: Vec<TypeExpr>,
        line: u32,
    },
    /// `check EXPR`: an invariant checked before the value is presented.
    Check(Expr),
    /// `summary "TEXT {EXPR} TEXT"`.
    Summary(Vec<Piece>),
    /// `field NAME = EXPR`: a named child.
    Field { name: String, value: Expr },
    /// `show SHAPE`.
    Show(Shape),
}

/// How many elements a sequence declares.
#[derive(Debug, Clone)]
pub enum Count {
    Known(Expr),
    /// `_`: the generator decides.
    Unknown,
}

/// What a view presents a value as.
#[derive(Debug, Clone)]
pub enum Shape {
    /// `text(PTR [, LEN])`.
    Text { pointer: Expr, length: Option<Expr> },
    /// `value(EXPR)`: present the value as another.
    Value(Expr),
    /// `empty("TEXT")`.
    Empty(String),
    /// `sequence(COUNT) for VAR in range(N) => ELEMENT`.
    Sequence {
        count: Count,
        variable: String,
        length: Expr,
        element: Expr,
    },
    /// `if COND { SHAPE } else { SHAPE }`.
    If {
        condition: Expr,
        then: Box<Self>,
        otherwise: Box<Self>,
    },
}

/// A path segment of a pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Name(String),
    /// `**`: any run of segments, including none.
    AnyRun,
}

/// A pattern of type identities: `path::base<arguments>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    pub path: Vec<Segment>,
    pub base: String,
    /// The arguments' patterns, when the pattern has an argument list. A
    /// type may have more arguments than its pattern lists.
    pub arguments: Option<Vec<ArgumentPattern>>,
}

/// A pattern of one argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgumentPattern {
    /// `_`: anything.
    Wildcard,
    /// A capital name, which captures the argument for the body.
    Capture(String),
    /// An integer argument's value.
    Value(IntegerValue),
    /// A type's pattern.
    Type(Pattern),
}

/// Parses one view file, named `source`.
#[must_use]
pub fn parse(source: &str, text: &str) -> File {
    let source: Arc<str> = source.into();
    let mut parser = Parser::new(&source, text);
    if text.len() > MAX_FILE_BYTES {
        return File {
            views: Vec::new(),
            errors: vec![parser.error_at(
                0,
                format!("a view file may be at most {MAX_FILE_BYTES} bytes long"),
            )],
        };
    }
    let mut views = Vec::new();
    let mut errors = Vec::new();
    if let Err(error) = parser.header() {
        return File {
            views,
            errors: vec![error],
        };
    }
    loop {
        parser.skip_blank();
        if parser.at_end() {
            break;
        }
        let start = parser.position;
        match parser.view() {
            Ok(view) => {
                if views.len() == MAX_VIEWS {
                    errors.push(parser.error_at(
                        start,
                        format!("a view file may hold at most {MAX_VIEWS} views"),
                    ));
                    break;
                }
                views.push(Arc::new(view));
            }
            Err(error) => {
                errors.push(error);
                // Resume at the next view.
                parser.position = start;
                if !parser.skip_to_next_view() {
                    break;
                }
            }
        }
    }
    File { views, errors }
}

struct Parser<'a> {
    source: &'a Arc<str>,
    text: &'a str,
    position: usize,
    /// The offset of each line's first byte.
    lines: Vec<usize>,
}

type Parsed<T> = Result<T, Error>;

const fn is_word_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

const fn is_word_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

impl<'a> Parser<'a> {
    fn new(source: &'a Arc<str>, text: &'a str) -> Self {
        let lines = std::iter::once(0)
            .chain(
                text.bytes()
                    .enumerate()
                    .filter(|(_, byte)| *byte == b'\n')
                    .map(|(index, _)| index + 1),
            )
            .collect();
        Self {
            source,
            text,
            position: 0,
            lines,
        }
    }

    /// The line and column of a byte offset, from one.
    fn location(&self, offset: usize) -> (u32, u32) {
        let line = self.lines.partition_point(|&start| start <= offset);
        let start = self.lines[line.saturating_sub(1)];
        let column = self
            .text
            .get(start..offset)
            .map_or(1, |prefix| prefix.chars().count() + 1);
        (
            u32::try_from(line).unwrap_or(u32::MAX),
            u32::try_from(column).unwrap_or(u32::MAX),
        )
    }

    fn error_at(&self, offset: usize, message: impl Into<String>) -> Error {
        let (line, column) = self.location(offset);
        Error {
            source: Arc::clone(self.source),
            line,
            column,
            message: message.into(),
        }
    }

    fn error(&self, message: impl Into<String>) -> Error {
        self.error_at(self.position, message)
    }

    fn rest(&self) -> &'a str {
        &self.text[self.position..]
    }

    const fn at_end(&self) -> bool {
        self.position >= self.text.len()
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.position).copied()
    }

    /// Skips spaces and comments on this line.
    fn skip_inline(&mut self) {
        while let Some(byte) = self.peek() {
            match byte {
                b' ' | b'\t' | b'\r' => self.position += 1,
                b'#' => self.skip_comment(),
                _ => break,
            }
        }
    }

    /// Skips spaces, comments, and line ends.
    fn skip_blank(&mut self) {
        while let Some(byte) = self.peek() {
            match byte {
                b' ' | b'\t' | b'\r' | b'\n' => self.position += 1,
                b'#' => self.skip_comment(),
                _ => break,
            }
        }
    }

    fn skip_comment(&mut self) {
        self.position = self
            .rest()
            .find('\n')
            .map_or(self.text.len(), |end| self.position + end);
    }

    /// The word at the cursor, without taking it.
    fn peek_word(&self) -> Option<&'a str> {
        let rest = self.rest();
        let bytes = rest.as_bytes();
        if !bytes.first().copied().is_some_and(is_word_start) {
            return None;
        }
        let length = bytes
            .iter()
            .take_while(|byte| is_word_continue(**byte))
            .count();
        Some(&rest[..length])
    }

    fn word(&mut self) -> Option<&'a str> {
        let word = self.peek_word()?;
        self.position += word.len();
        Some(word)
    }

    fn eat_word(&mut self, expected: &str) -> bool {
        if self.peek_word() == Some(expected) {
            self.position += expected.len();
            true
        } else {
            false
        }
    }

    fn expect_word(&mut self, expected: &str, context: &str) -> Parsed<()> {
        if self.eat_word(expected) {
            Ok(())
        } else {
            Err(self.unexpected(&format!("`{expected}` {context}")))
        }
    }

    fn eat(&mut self, expected: &str) -> bool {
        if self.rest().starts_with(expected) {
            self.position += expected.len();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: &str, context: &str) -> Parsed<()> {
        if self.eat(expected) {
            Ok(())
        } else {
            Err(self.unexpected(&format!("`{expected}` {context}")))
        }
    }

    fn unexpected(&self, expected: &str) -> Error {
        let found = match self.rest().chars().next() {
            None => "the end of the file".to_owned(),
            Some('\n') => "the end of the line".to_owned(),
            Some(_) => format!(
                "`{}`",
                self.peek_word()
                    .map_or_else(|| self.rest().chars().take(1).collect(), str::to_owned)
            ),
        };
        self.error(format!("expected {expected}, found {found}"))
    }

    /// A name a statement declares.
    fn name(&mut self, what: &str) -> Parsed<String> {
        self.skip_inline();
        let word = self.word();
        word.map(str::to_owned).ok_or_else(|| self.unexpected(what))
    }

    /// `uscope-views 1`.
    fn header(&mut self) -> Parsed<()> {
        self.skip_blank();
        let start = self.position;
        if !self.eat("uscope-views") {
            return Err(self.error_at(
                start,
                format!("a view file begins with `uscope-views {VERSION}`"),
            ));
        }
        self.skip_inline();
        let digits = self.rest().bytes().take_while(u8::is_ascii_digit).count();
        let version = self.rest()[..digits].parse::<u32>().ok();
        self.position += digits;
        if version != Some(VERSION) {
            return Err(self.error_at(
                start,
                format!("this uscope reads views of version {VERSION}"),
            ));
        }
        self.end_of_statement()
    }

    /// Moves past the view the cursor is in, to the next line that begins
    /// with `view`, and returns whether there is one.
    fn skip_to_next_view(&mut self) -> bool {
        // The view the cursor is at, or in, is skipped first.
        self.position += self.rest().chars().next().map_or(1, char::len_utf8);
        while !self.at_end() {
            let line_start = self.position == 0 || self.text.as_bytes()[self.position - 1] == b'\n';
            if line_start && self.peek_word() == Some("view") {
                return true;
            }
            self.position += self.rest().chars().next().map_or(1, char::len_utf8);
        }
        false
    }

    fn end_of_statement(&mut self) -> Parsed<()> {
        self.skip_inline();
        match self.peek() {
            None | Some(b'\n' | b'}') => Ok(()),
            Some(b';') => {
                self.position += 1;
                Ok(())
            }
            Some(_) => Err(self.unexpected("the end of the statement")),
        }
    }

    fn view(&mut self) -> Parsed<View> {
        let start = self.position;
        let line = self.location(start).0;
        match self.peek_word() {
            Some("view") => {}
            Some("extend") => {
                return Err(self.error("`extend` is not supported yet; write a whole `view`"));
            }
            _ => return Err(self.unexpected("`view`")),
        }
        self.position += "view".len();
        self.skip_inline();
        let language_start = self.position;
        let language_word = self
            .rest()
            .split(|character: char| character.is_whitespace())
            .next()
            .unwrap_or_default();
        let language = Language::parse(language_word).ok_or_else(|| {
            self.error("expected a language: `c`, `c++`, `rust`, `go`, `zig`, or `any`")
        })?;
        self.position += language_word.len();
        self.skip_inline();
        let pattern_start = self.position;
        let pattern_end = self.rest().find('{').map(|end| self.position + end);
        let Some(pattern_end) =
            pattern_end.filter(|end| !self.text[pattern_start..*end].contains('\n'))
        else {
            return Err(self.unexpected("a type pattern, then `{`"));
        };
        let pattern_text = self.text[pattern_start..pattern_end].trim();
        let pattern = parse_pattern(pattern_text, 0).map_err(|message| {
            self.error_at(pattern_start, format!("in the pattern: {message}"))
        })?;
        let header = format!(
            "{} {pattern_text}",
            &self.text[language_start..language_start + language_word.len()]
        );
        self.position = pattern_end + 1;
        let mut statements = Vec::new();
        let mut shown = false;
        loop {
            self.skip_blank();
            if self.eat("}") {
                break;
            }
            if self.at_end() {
                return Err(self.error_at(start, "the view is not closed with `}`"));
            }
            if statements.len() == MAX_STATEMENTS {
                return Err(self.error(format!(
                    "a view may hold at most {MAX_STATEMENTS} statements"
                )));
            }
            let statement_start = self.position;
            let statement = self.statement()?;
            if matches!(statement, Statement::Show(_)) {
                if shown {
                    return Err(self.error_at(statement_start, "a view shows its value once"));
                }
                shown = true;
            }
            statements.push(statement);
            self.end_of_statement()?;
        }
        if !shown {
            return Err(self.error_at(start, "a view needs a `show` statement"));
        }
        self.end_of_statement()?;
        Ok(View {
            source: Arc::clone(self.source),
            line,
            language,
            pattern,
            header: header.into(),
            statements,
        })
    }

    fn statement(&mut self) -> Parsed<Statement> {
        let start = self.position;
        let line = self.location(start).0;
        let Some(keyword) = self.peek_word() else {
            return Err(self.unexpected("a statement"));
        };
        match keyword {
            "let" => {
                self.position += keyword.len();
                let name = self.name("a name")?;
                self.skip_inline();
                self.expect("=", "after the name")?;
                let mut alternatives = vec![self.expression()?];
                while self.eat_word_after_blank("or") {
                    alternatives.push(self.expression()?);
                }
                Ok(Statement::Let {
                    name,
                    alternatives,
                    line,
                })
            }
            "type" => {
                self.position += keyword.len();
                let name = self.name("a name")?;
                self.skip_inline();
                self.expect("=", "after the name")?;
                let mut alternatives = vec![self.type_expr(0)?];
                while self.eat_word_after_blank("or") {
                    alternatives.push(self.type_expr(0)?);
                }
                Ok(Statement::Type {
                    name,
                    alternatives,
                    line,
                })
            }
            "check" => {
                self.position += keyword.len();
                Ok(Statement::Check(self.expression()?))
            }
            "summary" => {
                self.position += keyword.len();
                self.skip_inline();
                Ok(Statement::Summary(self.template()?))
            }
            "field" => {
                self.position += keyword.len();
                let name = self.name("a field's name")?;
                self.skip_inline();
                self.expect("=", "after the field's name")?;
                let value = self.expression()?;
                Ok(Statement::Field { name, value })
            }
            "show" => {
                self.position += keyword.len();
                Ok(Statement::Show(self.shape(0)?))
            }
            // A statement-level `if` chooses between shapes.
            "if" => Ok(Statement::Show(self.shape(0)?)),
            "hide" | "format" => Err(self.error(format!("`{keyword}` is not supported yet"))),
            _ => Err(self
                .unexpected("a statement: `let`, `type`, `check`, `summary`, `field`, or `show`")),
        }
    }

    /// Takes `word` when it is the next word, on this line or a later one.
    fn eat_word_after_blank(&mut self, word: &str) -> bool {
        let start = self.position;
        self.skip_blank();
        if self.eat_word(word) {
            return true;
        }
        self.position = start;
        false
    }

    #[expect(clippy::too_many_lines, reason = "one arm per shape")]
    fn shape(&mut self, depth: usize) -> Parsed<Shape> {
        if depth > MAX_SHAPE_DEPTH {
            return Err(self.error(format!("shapes may nest at most {MAX_SHAPE_DEPTH} deep")));
        }
        self.skip_inline();
        let Some(word) = self.peek_word() else {
            return Err(self.unexpected("a shape"));
        };
        match word {
            "text" => {
                self.position += word.len();
                self.open_call("text")?;
                let pointer = self.expression()?;
                self.skip_blank();
                let length = if self.eat(",") {
                    Some(self.expression()?)
                } else {
                    None
                };
                self.close_call("text")?;
                Ok(Shape::Text { pointer, length })
            }
            "value" => {
                self.position += word.len();
                self.open_call("value")?;
                let value = self.expression()?;
                self.close_call("value")?;
                Ok(Shape::Value(value))
            }
            "empty" => {
                self.position += word.len();
                self.open_call("empty")?;
                self.skip_blank();
                let text = self.string()?;
                self.close_call("empty")?;
                Ok(Shape::Empty(text))
            }
            "sequence" => {
                self.position += word.len();
                self.open_call("sequence")?;
                self.skip_blank();
                let count = if self.rest().starts_with('_')
                    && !self
                        .rest()
                        .as_bytes()
                        .get(1)
                        .copied()
                        .is_some_and(is_word_continue)
                {
                    self.position += 1;
                    Count::Unknown
                } else {
                    Count::Known(self.expression()?)
                };
                self.close_call("sequence")?;
                self.skip_blank();
                self.expect_word("for", "and a generator after the count")?;
                let variable = self.name("the generator's variable")?;
                self.skip_inline();
                self.expect_word("in", "after the generator's variable")?;
                self.skip_inline();
                match self.peek_word() {
                    Some("range") => self.position += "range".len(),
                    Some(other @ ("list" | "inorder")) => {
                        return Err(self.error(format!("`{other}` is not supported yet")));
                    }
                    _ => return Err(self.unexpected("`range(N)`")),
                }
                self.open_call("range")?;
                let length = self.expression()?;
                self.close_call("range")?;
                self.skip_blank();
                if self.peek_word() == Some("if") || self.peek_word() == Some("for") {
                    return Err(self.error("filters and nested generators are not supported yet"));
                }
                self.expect("=>", "before the element")?;
                let element = self.expression()?;
                Ok(Shape::Sequence {
                    count,
                    variable,
                    length,
                    element,
                })
            }
            "if" => {
                self.position += word.len();
                let condition = self.expression()?;
                let then = self.branch(depth)?;
                self.skip_blank();
                self.expect_word("else", "after the `if` branch")?;
                self.skip_blank();
                let otherwise = if self.peek_word() == Some("if") {
                    self.shape(depth + 1)?
                } else {
                    self.branch(depth)?
                };
                Ok(Shape::If {
                    condition,
                    then: Box::new(then),
                    otherwise: Box::new(otherwise),
                })
            }
            "map" | "dynamic" | "match" | "record" => {
                Err(self.error(format!("the `{word}` shape is not supported yet")))
            }
            _ => Err(self.unexpected("a shape: `text`, `value`, `empty`, `sequence`, or `if`")),
        }
    }

    /// `{ [show] SHAPE }`.
    fn branch(&mut self, depth: usize) -> Parsed<Shape> {
        self.skip_blank();
        self.expect("{", "to open the branch")?;
        self.skip_blank();
        self.eat_word("show");
        let shape = self.shape(depth + 1)?;
        self.skip_blank();
        self.expect("}", "to close the branch")?;
        Ok(shape)
    }

    fn open_call(&mut self, name: &str) -> Parsed<()> {
        self.skip_inline();
        self.expect("(", &format!("after `{name}`"))
    }

    fn close_call(&mut self, name: &str) -> Parsed<()> {
        self.skip_blank();
        self.expect(")", &format!("to close `{name}(`"))
    }

    /// A string literal: `"text"`, with the expression language's escapes.
    fn string(&mut self) -> Parsed<String> {
        let start = self.position;
        if !self.eat("\"") {
            return Err(self.unexpected("a string in double quotes"));
        }
        let mut text = String::new();
        loop {
            let Some(character) = self.rest().chars().next() else {
                return Err(self.error_at(start, "the string is not closed"));
            };
            self.position += character.len_utf8();
            match character {
                '"' => return Ok(text),
                '\n' => return Err(self.error_at(start, "the string is not closed")),
                '\\' => text.push(self.escape(start)?),
                character => text.push(character),
            }
        }
    }

    fn escape(&mut self, start: usize) -> Parsed<char> {
        let Some(character) = self.rest().chars().next() else {
            return Err(self.error_at(start, "the string is not closed"));
        };
        self.position += character.len_utf8();
        Ok(match character {
            'n' => '\n',
            't' => '\t',
            'r' => '\r',
            '0' => '\0',
            '\\' | '"' | '\'' | '{' | '}' => character,
            _ => {
                return Err(self.error_at(
                    self.position - character.len_utf8() - 1,
                    format!("`\\{character}` is not an escape"),
                ));
            }
        })
    }

    /// A summary's template: literal text and `{expression}` holes.
    fn template(&mut self) -> Parsed<Vec<Piece>> {
        let start = self.position;
        if !self.eat("\"") {
            return Err(self.unexpected("a summary in double quotes"));
        }
        let mut pieces = Vec::new();
        let mut literal = String::new();
        loop {
            let Some(character) = self.rest().chars().next() else {
                return Err(self.error_at(start, "the summary is not closed"));
            };
            match character {
                '"' => {
                    self.position += 1;
                    if !literal.is_empty() {
                        pieces.push(Piece::Literal(literal));
                    }
                    return Ok(pieces);
                }
                '\n' => return Err(self.error_at(start, "the summary is not closed")),
                '\\' => {
                    self.position += 1;
                    literal.push(self.escape(start)?);
                }
                '{' => {
                    self.position += 1;
                    if !literal.is_empty() {
                        pieces.push(Piece::Literal(std::mem::take(&mut literal)));
                    }
                    let end = self.rest().find('}').ok_or_else(|| {
                        self.error("the `{` in the summary is not closed with `}`")
                    })?;
                    let hole_start = self.position;
                    let hole = &self.text[hole_start..hole_start + end];
                    if hole.contains(['"', '\n']) {
                        return Err(self.error("a summary's `{…}` holds one expression"));
                    }
                    pieces.push(Piece::Hole(
                        self.expression_text(hole_start, hole_start + end)?,
                    ));
                    self.position = hole_start + end + 1;
                }
                '}' => return Err(self.error("write `\\}` for a `}` in a summary")),
                character => {
                    self.position += character.len_utf8();
                    literal.push(character);
                }
            }
        }
    }

    /// A type for a `type` statement.
    fn type_expr(&mut self, depth: usize) -> Parsed<TypeExpr> {
        if depth > MAX_PATTERN_DEPTH {
            return Err(self.error("the type nests too deeply"));
        }
        self.skip_inline();
        match self.peek_word() {
            Some("typeof") if self.rest()["typeof".len()..].trim_start().starts_with('(') => {
                self.position += "typeof".len();
                self.open_call("typeof")?;
                let expression = self.expression()?;
                self.close_call("typeof")?;
                Ok(TypeExpr::TypeOf(expression))
            }
            Some("arg") if self.rest()["arg".len()..].trim_start().starts_with('(') => {
                self.position += "arg".len();
                self.open_call("arg")?;
                let of = self.type_expr(depth + 1)?;
                self.skip_blank();
                self.expect(",", "between `arg`'s type and position")?;
                self.skip_blank();
                let digits = self.rest().bytes().take_while(u8::is_ascii_digit).count();
                let index = self.rest()[..digits]
                    .parse::<u32>()
                    .map_err(|_| self.unexpected("an argument's position"))?;
                self.position += digits;
                self.close_call("arg")?;
                Ok(TypeExpr::Arg {
                    of: Box::new(of),
                    index,
                })
            }
            _ => {
                let start = self.position;
                let end = self.scan_type_name();
                let text = self.text[start..end].trim();
                if text.is_empty() {
                    return Err(self.unexpected("a type"));
                }
                self.position = end;
                let name = text.trim_end_matches(|character: char| {
                    character == '*' || character.is_whitespace()
                });
                let pointers = text[name.len()..].matches('*').count();
                Ok(TypeExpr::Named {
                    name: name.to_owned(),
                    pointers: u8::try_from(pointers).unwrap_or(u8::MAX),
                })
            }
        }
    }

    /// The end of a type's name: before `or`, `,`, or `)` outside its
    /// brackets, or the end of the line.
    fn scan_type_name(&self) -> usize {
        let bytes = self.text.as_bytes();
        let mut depth = 0_usize;
        let mut index = self.position;
        while let Some(&byte) = bytes.get(index) {
            match byte {
                b'`' => {
                    index += 1 + self.text[index + 1..].find('`').unwrap_or(self.text.len());
                }
                b'<' | b'(' | b'[' => depth += 1,
                b'>' | b']' => depth = depth.saturating_sub(1),
                b')' | b',' | b'\n' | b'#' if depth == 0 => return index,
                b')' => depth -= 1,
                byte if depth == 0
                    && is_word_start(byte)
                    && (index == 0 || !is_word_continue(bytes[index - 1]))
                    && self.text[index..].starts_with("or")
                    && !bytes.get(index + 2).copied().is_some_and(is_word_continue) =>
                {
                    return index;
                }
                _ => {}
            }
            index += 1;
        }
        index.min(self.text.len())
    }

    /// The expression from the cursor to where it ends.
    fn expression(&mut self) -> Parsed<Expr> {
        self.skip_inline();
        let start = self.position;
        let end = self.scan_expression();
        let expression = self.expression_text(start, end)?;
        self.position = end;
        Ok(expression)
    }

    /// Parses the text between two offsets, with comments blanked out, as
    /// a view's expression.
    fn expression_text(&self, start: usize, end: usize) -> Parsed<Expr> {
        let raw = &self.text[start..end];
        if raw.trim().is_empty() {
            return Err(self.error_at(start, "expected an expression"));
        }
        let text = blank_comments(raw);
        let line = self.location(start).0;
        Expression::parse_view(&text)
            .map(|expression| Expr { expression, line })
            .map_err(|error| {
                let at = start + (error.span.start as usize).min(raw.len());
                self.error_at(at, error.to_string())
            })
    }

    /// Where the expression at the cursor ends.
    fn scan_expression(&self) -> usize {
        let bytes = self.text.as_bytes();
        let mut depth = 0_usize;
        let mut index = self.position;
        let mut end = index;
        while let Some(&byte) = bytes.get(index) {
            match byte {
                b'"' | b'\'' => {
                    index = skip_literal(self.text, index, byte);
                    end = index;
                    continue;
                }
                b'`' => {
                    index = self.text[index + 1..]
                        .find('`')
                        .map_or(self.text.len(), |close| index + close + 2);
                    end = index;
                    continue;
                }
                b'#' => {
                    index = self.text[index..]
                        .find('\n')
                        .map_or(self.text.len(), |line_end| index + line_end);
                    continue;
                }
                b'(' | b'[' => depth += 1,
                b')' | b']' | b'{' | b'}' | b',' | b';' if depth == 0 => return end,
                b')' | b']' => depth -= 1,
                b'=' if depth == 0 && bytes.get(index + 1) == Some(&b'>') => return end,
                b'\n' if depth == 0 && self.statement_ends_before(index + 1) => return end,
                byte if is_word_start(byte)
                    && (index == 0 || !is_word_continue(bytes[index - 1])) =>
                {
                    let length = bytes[index..]
                        .iter()
                        .take_while(|byte| is_word_continue(**byte))
                        .count();
                    let word = &self.text[index..index + length];
                    if depth == 0 && STOP_WORDS.contains(&word) {
                        return end;
                    }
                    index += length;
                    end = index;
                    continue;
                }
                _ => {}
            }
            index += 1;
            if !matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
                end = index;
            }
        }
        end
    }

    /// Whether the line at `offset`, or the next line that is not blank,
    /// begins a statement or closes a view, which ends an expression.
    fn statement_ends_before(&self, offset: usize) -> bool {
        let mut rest = &self.text[offset.min(self.text.len())..];
        loop {
            let trimmed = rest.trim_start_matches([' ', '\t', '\r']);
            if let Some(after) = trimmed.strip_prefix('\n') {
                rest = after;
                continue;
            }
            if trimmed.starts_with('#') {
                match trimmed.find('\n') {
                    Some(line_end) => {
                        rest = &trimmed[line_end + 1..];
                        continue;
                    }
                    None => return true,
                }
            }
            if trimmed.is_empty() || trimmed.starts_with('}') {
                return true;
            }
            let length = trimmed
                .bytes()
                .take_while(|byte| is_word_continue(*byte))
                .count();
            return STATEMENTS.contains(&&trimmed[..length]);
        }
    }
}

/// The offset after a string or character literal that begins at `start`.
fn skip_literal(text: &str, start: usize, quote: u8) -> usize {
    let bytes = text.as_bytes();
    let mut index = start + 1;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'\\' => index += 2,
            b'\n' => return index,
            byte if byte == quote => return index + 1,
            _ => index += 1,
        }
    }
    text.len()
}

/// The text with every comment replaced by spaces, so its offsets stay.
fn blank_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            quote @ (b'"' | b'\'') => {
                let end = skip_literal(text, index, quote).min(text.len());
                out.push_str(&text[index..end]);
                index = end;
            }
            b'`' => {
                let end = text[index + 1..]
                    .find('`')
                    .map_or(text.len(), |close| index + close + 2);
                out.push_str(&text[index..end]);
                index = end;
            }
            b'#' => {
                let end = text[index..]
                    .find('\n')
                    .map_or(text.len(), |line_end| index + line_end);
                out.extend(std::iter::repeat_n(' ', end - index));
                index = end;
            }
            _ => {
                let character = text[index..].chars().next().expect("a character");
                out.push(character);
                index += character.len_utf8();
            }
        }
    }
    out
}

/// Parses a type pattern.
fn parse_pattern(text: &str, depth: usize) -> Result<Pattern, String> {
    if depth > MAX_PATTERN_DEPTH {
        return Err("the pattern nests too deeply".to_owned());
    }
    let text = text.trim();
    if text.is_empty() {
        return Err("expected a type pattern".to_owned());
    }
    let (qualified, arguments) = split_arguments(text)?;
    let mut segments = split_path(qualified.trim())?;
    let base = match segments.pop() {
        Some(Segment::Name(base)) => base,
        Some(Segment::AnyRun) => return Err("`**` cannot end a pattern".to_owned()),
        None => return Err("expected a type's name".to_owned()),
    };
    let arguments = arguments
        .map(|arguments| {
            arguments
                .into_iter()
                .map(|argument| parse_argument(argument, depth))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    Ok(Pattern {
        path: segments,
        base,
        arguments,
    })
}

fn parse_argument(text: &str, depth: usize) -> Result<ArgumentPattern, String> {
    let text = text.trim();
    if text == "_" {
        return Ok(ArgumentPattern::Wildcard);
    }
    if let Some(value) = crate::type_identity::parse_integer(text) {
        return Ok(ArgumentPattern::Value(value));
    }
    let capture = text.starts_with(|character: char| character.is_ascii_uppercase())
        && text.bytes().all(is_word_continue);
    if capture {
        return Ok(ArgumentPattern::Capture(text.to_owned()));
    }
    parse_pattern(text, depth + 1).map(ArgumentPattern::Type)
}

/// Splits `a::b<x, y>` into `a::b` and its arguments, in `<>`, `()`, or
/// `[]`.
fn split_arguments(text: &str) -> Result<(&str, Option<Vec<&str>>), String> {
    let Some(open) = outside_backticks(text)
        .find(|(_, character)| matches!(character, '<' | '(' | '['))
        .map(|(index, _)| index)
    else {
        return Ok((text, None));
    };
    let close = match text.as_bytes()[open] {
        b'<' => '>',
        b'(' => ')',
        _ => ']',
    };
    let inner = text[open + 1..]
        .strip_suffix(close)
        .ok_or_else(|| format!("the arguments must end the pattern with `{close}`"))?;
    let mut arguments = Vec::new();
    let mut depth = 0_usize;
    let mut start = 0;
    for (index, character) in inner.char_indices() {
        match character {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| "the pattern's brackets do not balance".to_owned())?;
            }
            ',' if depth == 0 => {
                arguments.push(&inner[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err("the pattern's brackets do not balance".to_owned());
    }
    if !inner.trim().is_empty() {
        arguments.push(&inner[start..]);
    }
    if arguments.iter().any(|argument| argument.trim().is_empty()) {
        return Err("an argument is empty".to_owned());
    }
    Ok((&text[..open], Some(arguments)))
}

/// The characters of `text` outside backticks, with their offsets.
fn outside_backticks(text: &str) -> impl Iterator<Item = (usize, char)> + '_ {
    let mut quoted = false;
    text.char_indices().filter(move |(_, character)| {
        if *character == '`' {
            quoted = !quoted;
            return false;
        }
        !quoted
    })
}

/// Splits a path at `::` or `.` outside backticks.
fn split_path(text: &str) -> Result<Vec<Segment>, String> {
    let text = text.strip_prefix("::").unwrap_or(text);
    let mut parts = Vec::new();
    let mut start = 0;
    let mut skip = 0;
    for (index, character) in outside_backticks(text) {
        if index < skip {
            continue;
        }
        if text[index..].starts_with("::") {
            parts.push(&text[start..index]);
            start = index + 2;
            skip = start;
        } else if character == '.' && !text[index..].starts_with("**") {
            parts.push(&text[start..index]);
            start = index + 1;
        }
    }
    parts.push(&text[start..]);
    let mut segments = Vec::new();
    for part in parts {
        let part = part.trim();
        if part == "**" {
            if segments.last() == Some(&Segment::AnyRun) {
                return Err("`**` follows `**`".to_owned());
            }
            segments.push(Segment::AnyRun);
            continue;
        }
        let name = part
            .strip_prefix('`')
            .and_then(|quoted| quoted.strip_suffix('`'))
            .unwrap_or(part);
        let plain = part.starts_with('`')
            || name.bytes().next().is_some_and(is_word_start) && name.bytes().all(is_word_continue);
        if name.is_empty() || !plain {
            return Err(format!("`{part}` is not a name"));
        }
        segments.push(Segment::Name(name.to_owned()));
    }
    Ok(segments)
}
