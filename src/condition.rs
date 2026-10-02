//! Breakpoint conditions and log messages.
//!
//! A condition is a C-like expression over values of the stopped frame:
//! paths such as `node->next.value` or `items[i]`, integer, floating-point,
//! character, and boolean literals, arithmetic, comparisons, and the logical
//! operators, which short-circuit so `p != NULL && p->x > 3` is safe. A log
//! message is text with value paths in braces: `x = {x}, y = {p->y}`.

use std::fmt;
use std::sync::Arc;

use crate::{Error, Result, ValueExpression, ValuePathStep};

const MAX_TEXT_BYTES: usize = 4096;
const MAX_DEPTH: usize = 64;

/// A parsed breakpoint condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    text: Arc<str>,
    expression: Arc<Expression>,
}

/// One evaluated operand of a condition.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Operand {
    Integer(i128),
    Float(f64),
    Boolean(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Expression {
    Integer(i128),
    /// A floating-point literal, by its bits.
    Float(u64),
    Boolean(bool),
    Path {
        root: String,
        selectors: Vec<Selector>,
    },
    Unary(UnaryOperator, Box<Self>),
    Binary(BinaryOperator, Box<Self>, Box<Self>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Selector {
    Member(String),
    Index(Box<Expression>),
    Dereference,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnaryOperator {
    Not,
    Negate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BinaryOperator {
    Or,
    And,
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
}

impl Condition {
    /// Parses a condition.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        if text.is_empty() {
            return Err(invalid("a condition must not be empty"));
        }
        if text.len() > MAX_TEXT_BYTES {
            return Err(invalid("the condition is too long"));
        }
        let mut parser = Parser {
            tokens: tokenize(text)?,
            position: 0,
            depth: 0,
        };
        let expression = parser.or()?;
        if let Some(token) = parser.tokens.get(parser.position) {
            return Err(invalid(format!("unexpected {token} in the condition")));
        }
        Ok(Self {
            text: text.into(),
            expression: Arc::new(expression),
        })
    }

    /// Evaluates the expression to a value rather than a truth value.
    pub fn value(
        &self,
        resolve: &mut dyn FnMut(&ValueExpression) -> std::result::Result<Operand, String>,
    ) -> std::result::Result<Operand, String> {
        evaluate(&self.expression, resolve)
    }

    /// Evaluates the condition, resolving each value path with `resolve`.
    pub fn evaluate(
        &self,
        resolve: &mut dyn FnMut(&ValueExpression) -> std::result::Result<Operand, String>,
    ) -> std::result::Result<bool, String> {
        evaluate(&self.expression, resolve).map(truth)
    }
}

impl Operand {
    /// The operand an inspected value gives a condition: its number, truth
    /// value, or address. Aggregates and values that could not be read have
    /// none, and say why.
    pub fn of(
        path: &ValueExpression,
        type_info: Option<&crate::TypeInfo>,
        state: &crate::VariableState,
    ) -> std::result::Result<Self, String> {
        use crate::{FloatValue, IntegerValue, ScalarValue, VariableState, VariableValue};

        let integer = |value: IntegerValue| match value {
            IntegerValue::Signed(value) => Ok(Self::Integer(value)),
            IntegerValue::Unsigned(value) => i128::try_from(value)
                .map(Self::Integer)
                .map_err(|_| format!("{path} is too large to compare")),
        };
        match state {
            VariableState::Available { value, .. } => match value {
                VariableValue::Scalar(ScalarValue::Boolean(value)) => Ok(Self::Boolean(*value)),
                VariableValue::Scalar(ScalarValue::Signed(value)) => Ok(Self::Integer(*value)),
                VariableValue::Scalar(ScalarValue::Unsigned(value)) => {
                    integer(IntegerValue::Unsigned(*value))
                }
                VariableValue::Scalar(ScalarValue::Floating(value)) => {
                    Ok(Self::Float(match value {
                        FloatValue::Binary32(bits) => f64::from(f32::from_bits(*bits)),
                        FloatValue::Binary64(bits) => f64::from_bits(*bits),
                        FloatValue::X87Extended {
                            significand,
                            sign_exponent,
                        } => x87_to_f64(*significand, *sign_exponent),
                    }))
                }
                VariableValue::Enumeration { value, .. } => integer(*value),
                VariableValue::Address(address) => {
                    Ok(Self::Integer(i128::from(address.address.get())))
                }
                _ => Err(format!(
                    "{path} is a {}; compare one of its members or elements",
                    type_info.map_or("value without a number", |info| &info.name)
                )),
            },
            VariableState::Unavailable(reason) => Err(format!("{path} is unavailable: {reason}")),
            VariableState::Invalid { reason, .. } => Err(format!("{path} is invalid: {reason}")),
            VariableState::Malformed(reason) => {
                Err(format!("{path} is malformed: {}", reason.description))
            }
        }
    }
}

/// Converts an x87 extended value to the nearest double.
fn x87_to_f64(significand: u64, sign_exponent: u16) -> f64 {
    let sign = if sign_exponent & 0x8000 == 0 {
        1.0
    } else {
        -1.0
    };
    let exponent = i32::from(sign_exponent & 0x7fff);
    if exponent == 0x7fff {
        return if significand << 1 == 0 {
            sign * f64::INFINITY
        } else {
            f64::NAN
        };
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "a double holds the nearest value of the 64-bit significand"
    )]
    let magnitude = significand as f64;
    sign * magnitude * 2_f64.powi(exponent - 16383 - 63)
}

impl fmt::Display for Condition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.text)
    }
}

impl std::str::FromStr for Condition {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        Self::parse(text)
    }
}

/// A logpoint's message: text with values interpolated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogMessage {
    text: Arc<str>,
    segments: Arc<[LogSegment]>,
}

/// One piece of a log message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogSegment {
    /// Literal text.
    Text(Arc<str>),
    /// A value path whose value is shown in its place.
    Value(ValueExpression),
}

impl LogMessage {
    /// Parses a message whose `{path}` parts show values; `{{` and `}}`
    /// stand for braces.
    pub fn parse(text: &str) -> Result<Self> {
        if text.len() > MAX_TEXT_BYTES {
            return Err(invalid_log("the message is too long"));
        }
        let mut segments = Vec::new();
        let mut literal = String::new();
        let mut rest = text;
        while let Some(index) = rest.find(['{', '}']) {
            literal.push_str(&rest[..index]);
            let brace = &rest[index..];
            if brace.starts_with("{{") || brace.starts_with("}}") {
                literal.push_str(&brace[..1]);
                rest = &brace[2..];
                continue;
            }
            if brace.starts_with('}') {
                return Err(invalid_log("a '}' has no '{'; write '}}' for a brace"));
            }
            let Some(end) = brace.find('}') else {
                return Err(invalid_log("a '{' has no '}'; write '{{' for a brace"));
            };
            let path = brace[1..end].trim();
            let value = Condition::parse(path)
                .ok()
                .and_then(|condition| constant_path(&condition.expression))
                .ok_or_else(|| {
                    invalid_log(format!(
                        "'{{{path}}}' is not a value path such as name, a.b, p->next, or items[2]"
                    ))
                })?;
            if !literal.is_empty() {
                segments.push(LogSegment::Text(std::mem::take(&mut literal).into()));
            }
            segments.push(LogSegment::Value(value));
            rest = &brace[end + 1..];
        }
        literal.push_str(rest);
        if !literal.is_empty() {
            segments.push(LogSegment::Text(literal.into()));
        }
        Ok(Self {
            text: text.into(),
            segments: segments.into(),
        })
    }

    /// The message's parts in order.
    #[must_use]
    pub fn segments(&self) -> &[LogSegment] {
        &self.segments
    }
}

impl fmt::Display for LogMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.text)
    }
}

fn invalid(description: impl Into<String>) -> Error {
    Error::InvalidCondition(description.into())
}

fn invalid_log(description: impl Into<String>) -> Error {
    Error::InvalidLogMessage(description.into())
}

/// The value path an expression names when it is only a path with
/// constant indices.
fn constant_path(expression: &Expression) -> Option<ValueExpression> {
    let Expression::Path { root, selectors } = expression else {
        return None;
    };
    let mut steps = vec![ValuePathStep::Named(root.clone())];
    for selector in selectors {
        steps.push(match selector {
            Selector::Member(name) => ValuePathStep::Named(name.clone()),
            Selector::Dereference => ValuePathStep::Dereference,
            Selector::Index(index) => match index.as_ref() {
                Expression::Integer(index) => ValuePathStep::Index(*index),
                _ => return None,
            },
        });
    }
    Some(ValueExpression {
        steps: steps.into(),
    })
}

fn truth(operand: Operand) -> bool {
    match operand {
        Operand::Integer(value) => value != 0,
        Operand::Float(value) => value != 0.0,
        Operand::Boolean(value) => value,
    }
}

fn evaluate(
    expression: &Expression,
    resolve: &mut dyn FnMut(&ValueExpression) -> std::result::Result<Operand, String>,
) -> std::result::Result<Operand, String> {
    Ok(match expression {
        Expression::Integer(value) => Operand::Integer(*value),
        Expression::Float(bits) => Operand::Float(f64::from_bits(*bits)),
        Expression::Boolean(value) => Operand::Boolean(*value),
        Expression::Path { root, selectors } => {
            let mut steps = vec![ValuePathStep::Named(root.clone())];
            for selector in selectors {
                steps.push(match selector {
                    Selector::Member(name) => ValuePathStep::Named(name.clone()),
                    Selector::Dereference => ValuePathStep::Dereference,
                    Selector::Index(index) => match evaluate(index, resolve)? {
                        Operand::Integer(index) => ValuePathStep::Index(index),
                        _ => return Err("an index must be an integer".to_owned()),
                    },
                });
            }
            resolve(&ValueExpression {
                steps: steps.into(),
            })?
        }
        Expression::Unary(operator, operand) => {
            let operand = evaluate(operand, resolve)?;
            match operator {
                UnaryOperator::Not => Operand::Boolean(!truth(operand)),
                UnaryOperator::Negate => match operand {
                    Operand::Integer(value) => Operand::Integer(
                        value
                            .checked_neg()
                            .ok_or_else(|| "the negation overflows".to_owned())?,
                    ),
                    Operand::Float(value) => Operand::Float(-value),
                    Operand::Boolean(_) => return Err("a boolean cannot be negated".to_owned()),
                },
            }
        }
        Expression::Binary(BinaryOperator::Or, left, right) => {
            Operand::Boolean(truth(evaluate(left, resolve)?) || truth(evaluate(right, resolve)?))
        }
        Expression::Binary(BinaryOperator::And, left, right) => {
            Operand::Boolean(truth(evaluate(left, resolve)?) && truth(evaluate(right, resolve)?))
        }
        Expression::Binary(operator, left, right) => binary(
            *operator,
            evaluate(left, resolve)?,
            evaluate(right, resolve)?,
        )?,
    })
}

fn binary(
    operator: BinaryOperator,
    left: Operand,
    right: Operand,
) -> std::result::Result<Operand, String> {
    let number = |operand| match operand {
        Operand::Boolean(value) => Operand::Integer(i128::from(value)),
        other => other,
    };
    let (left, right) = (number(left), number(right));
    let compare = |ordering: Option<std::cmp::Ordering>| {
        let ordering = ordering.ok_or_else(|| "a NaN cannot be compared".to_owned())?;
        Ok(Operand::Boolean(match operator {
            BinaryOperator::Equal => ordering.is_eq(),
            BinaryOperator::NotEqual => ordering.is_ne(),
            BinaryOperator::Less => ordering.is_lt(),
            BinaryOperator::LessOrEqual => ordering.is_le(),
            BinaryOperator::Greater => ordering.is_gt(),
            _ => ordering.is_ge(),
        }))
    };
    match (left, right) {
        (Operand::Integer(left), Operand::Integer(right)) => match operator {
            BinaryOperator::Equal
            | BinaryOperator::NotEqual
            | BinaryOperator::Less
            | BinaryOperator::LessOrEqual
            | BinaryOperator::Greater
            | BinaryOperator::GreaterOrEqual => compare(Some(left.cmp(&right))),
            BinaryOperator::Add => left
                .checked_add(right)
                .map(Operand::Integer)
                .ok_or_else(overflow),
            BinaryOperator::Subtract => left
                .checked_sub(right)
                .map(Operand::Integer)
                .ok_or_else(overflow),
            BinaryOperator::Multiply => left
                .checked_mul(right)
                .map(Operand::Integer)
                .ok_or_else(overflow),
            BinaryOperator::Divide | BinaryOperator::Remainder if right == 0 => {
                Err("the condition divides by zero".to_owned())
            }
            BinaryOperator::Divide => left
                .checked_div(right)
                .map(Operand::Integer)
                .ok_or_else(overflow),
            BinaryOperator::Remainder => left
                .checked_rem(right)
                .map(Operand::Integer)
                .ok_or_else(overflow),
            BinaryOperator::Or | BinaryOperator::And => unreachable!("logic short-circuits"),
        },
        (left, right) => {
            let float = |operand| match operand {
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "mixed arithmetic is floating-point arithmetic, as in C"
                )]
                Operand::Integer(value) => value as f64,
                Operand::Float(value) => value,
                Operand::Boolean(value) => f64::from(u8::from(value)),
            };
            let (left, right) = (float(left), float(right));
            match operator {
                BinaryOperator::Add => Ok(Operand::Float(left + right)),
                BinaryOperator::Subtract => Ok(Operand::Float(left - right)),
                BinaryOperator::Multiply => Ok(Operand::Float(left * right)),
                BinaryOperator::Divide => Ok(Operand::Float(left / right)),
                BinaryOperator::Remainder => Ok(Operand::Float(left % right)),
                _ => compare(left.partial_cmp(&right)),
            }
        }
    }
}

fn overflow() -> String {
    "the arithmetic overflows".to_owned()
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Integer(i128),
    Float(f64),
    Character(i128),
    Name(String),
    Symbol(&'static str),
}

impl fmt::Display for Token {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integer(value) | Self::Character(value) => write!(formatter, "'{value}'"),
            Self::Float(value) => write!(formatter, "'{value}'"),
            Self::Name(name) => write!(formatter, "'{name}'"),
            Self::Symbol(symbol) => write!(formatter, "'{symbol}'"),
        }
    }
}

const SYMBOLS: [&str; 22] = [
    "->", "==", "!=", "<=", ">=", "&&", "||", "<", ">", "!", "+", "-", "*", "/", "%", "(", ")",
    "[", "]", ".", "&", "|",
];

fn tokenize(text: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut rest = text;
    loop {
        rest = rest.trim_start();
        let Some(first) = rest.chars().next() else {
            return Ok(tokens);
        };
        if first.is_ascii_digit() {
            let length = rest
                .find(|character: char| !(character.is_ascii_alphanumeric() || character == '.'))
                .unwrap_or(rest.len());
            tokens.push(number(&rest[..length])?);
            rest = &rest[length..];
        } else if first == '\'' {
            let (value, length) = character(rest)?;
            tokens.push(Token::Character(value));
            rest = &rest[length..];
        } else if first.is_alphabetic() || first == '_' || first == '$' {
            let mut length = 0;
            for (index, character) in rest.char_indices() {
                if character.is_alphanumeric() || character == '_' || character == '$' {
                    length = index + character.len_utf8();
                } else if character == ':' && rest[index..].starts_with("::") {
                    length = index + 2;
                } else if character == ':' && rest[..index].ends_with(':') {
                    // The second colon of a `::` already taken.
                } else {
                    break;
                }
            }
            tokens.push(Token::Name(rest[..length].to_owned()));
            rest = &rest[length..];
        } else if let Some(symbol) = SYMBOLS.iter().find(|symbol| rest.starts_with(**symbol)) {
            if matches!(*symbol, "&" | "|") {
                return Err(invalid(format!(
                    "'{symbol}' is not supported; conditions use '&&' and '||'"
                )));
            }
            tokens.push(Token::Symbol(symbol));
            rest = &rest[symbol.len()..];
        } else if first == '=' {
            return Err(invalid("'=' assigns; compare with '=='"));
        } else {
            return Err(invalid(format!("unexpected '{first}' in the condition")));
        }
    }
}

fn number(text: &str) -> Result<Token> {
    let bad = || invalid(format!("'{text}' is not a number"));
    let lower = text.to_ascii_lowercase();
    if let Some(digits) = lower.strip_prefix("0x") {
        let digits = digits.trim_end_matches(['u', 'l']);
        return i128::from_str_radix(digits, 16)
            .map(Token::Integer)
            .map_err(|_| bad());
    }
    if lower.contains(['.', 'e']) && !lower.starts_with("0b") {
        return lower
            .trim_end_matches('f')
            .parse::<f64>()
            .map(Token::Float)
            .map_err(|_| bad());
    }
    if let Some(digits) = lower.strip_prefix("0b") {
        return i128::from_str_radix(digits, 2)
            .map(Token::Integer)
            .map_err(|_| bad());
    }
    lower
        .trim_end_matches(['u', 'l'])
        .parse::<i128>()
        .map(Token::Integer)
        .map_err(|_| bad())
}

/// Reads a character literal such as `'a'` or `'\n'`, returning its value
/// and length.
fn character(text: &str) -> Result<(i128, usize)> {
    let bad = || invalid("a character literal is malformed");
    let mut characters = text.char_indices().skip(1);
    let (_, first) = characters.next().ok_or_else(bad)?;
    let value = if first == '\\' {
        let (_, escaped) = characters.next().ok_or_else(bad)?;
        match escaped {
            'n' => '\n',
            't' => '\t',
            'r' => '\r',
            '0' => '\0',
            '\\' | '\'' | '"' => escaped,
            _ => return Err(bad()),
        }
    } else {
        first
    };
    let (end, quote) = characters.next().ok_or_else(bad)?;
    if quote != '\'' {
        return Err(bad());
    }
    Ok((i128::from(u32::from(value)), end + 1))
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn take_symbol(&mut self, symbols: &[&'static str]) -> Option<&'static str> {
        match self.peek() {
            Some(Token::Symbol(symbol)) if symbols.contains(symbol) => {
                let symbol = *symbol;
                self.position += 1;
                Some(symbol)
            }
            _ => None,
        }
    }

    fn expect(&mut self, symbol: &'static str) -> Result<()> {
        if self.take_symbol(&[symbol]).is_some() {
            Ok(())
        } else {
            Err(invalid(self.peek().map_or_else(
                || format!("expected '{symbol}' at the end"),
                |token| format!("expected '{symbol}' but found {token}"),
            )))
        }
    }

    fn nested<T>(&mut self, parse: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(invalid("the condition nests too deeply"));
        }
        let result = parse(self);
        self.depth -= 1;
        result
    }

    fn or(&mut self) -> Result<Expression> {
        let mut left = self.and()?;
        while self.take_symbol(&["||"]).is_some() {
            let right = self.and()?;
            left = Expression::Binary(BinaryOperator::Or, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expression> {
        let mut left = self.comparison()?;
        while self.take_symbol(&["&&"]).is_some() {
            let right = self.comparison()?;
            left = Expression::Binary(BinaryOperator::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn comparison(&mut self) -> Result<Expression> {
        const COMPARISONS: [&str; 6] = ["==", "!=", "<=", ">=", "<", ">"];
        let left = self.sum()?;
        let Some(symbol) = self.take_symbol(&COMPARISONS) else {
            return Ok(left);
        };
        let operator = match symbol {
            "==" => BinaryOperator::Equal,
            "!=" => BinaryOperator::NotEqual,
            "<=" => BinaryOperator::LessOrEqual,
            ">=" => BinaryOperator::GreaterOrEqual,
            "<" => BinaryOperator::Less,
            _ => BinaryOperator::Greater,
        };
        let right = self.sum()?;
        if self.take_symbol(&COMPARISONS).is_some() {
            return Err(invalid(
                "comparisons cannot be chained; join them with '&&'",
            ));
        }
        Ok(Expression::Binary(
            operator,
            Box::new(left),
            Box::new(right),
        ))
    }

    fn sum(&mut self) -> Result<Expression> {
        let mut left = self.product()?;
        while let Some(symbol) = self.take_symbol(&["+", "-"]) {
            let operator = if symbol == "+" {
                BinaryOperator::Add
            } else {
                BinaryOperator::Subtract
            };
            let right = self.product()?;
            left = Expression::Binary(operator, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn product(&mut self) -> Result<Expression> {
        let mut left = self.unary()?;
        while let Some(symbol) = self.take_symbol(&["*", "/", "%"]) {
            let operator = match symbol {
                "*" => BinaryOperator::Multiply,
                "/" => BinaryOperator::Divide,
                _ => BinaryOperator::Remainder,
            };
            let right = self.unary()?;
            left = Expression::Binary(operator, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expression> {
        self.nested(|parser| match parser.take_symbol(&["!", "-", "*"]) {
            Some("!") => Ok(Expression::Unary(
                UnaryOperator::Not,
                Box::new(parser.unary()?),
            )),
            Some("-") => Ok(Expression::Unary(
                UnaryOperator::Negate,
                Box::new(parser.unary()?),
            )),
            Some(_) => match parser.unary()? {
                Expression::Path {
                    root,
                    mut selectors,
                } => {
                    selectors.push(Selector::Dereference);
                    Ok(Expression::Path { root, selectors })
                }
                _ => Err(invalid("only a value can be dereferenced")),
            },
            None => parser.postfix(),
        })
    }

    fn postfix(&mut self) -> Result<Expression> {
        let mut expression = self.primary()?;
        loop {
            let Some(symbol) = self.take_symbol(&[".", "->", "["]) else {
                return Ok(expression);
            };
            let Expression::Path { selectors, .. } = &mut expression else {
                return Err(invalid(format!("'{symbol}' must follow a value")));
            };
            if symbol == "[" {
                let index = self.nested(Self::or)?;
                self.expect("]")?;
                selectors.push(Selector::Index(Box::new(index)));
            } else {
                {
                    if symbol == "->" {
                        selectors.push(Selector::Dereference);
                    }
                    match self.tokens.get(self.position) {
                        Some(Token::Name(name)) => {
                            selectors.push(Selector::Member(name.clone()));
                            self.position += 1;
                        }
                        _ => {
                            return Err(invalid(format!(
                                "'{symbol}' must be followed by a member name"
                            )));
                        }
                    }
                }
            }
        }
    }

    fn primary(&mut self) -> Result<Expression> {
        let token = self
            .tokens
            .get(self.position)
            .cloned()
            .ok_or_else(|| invalid("the condition ends early"))?;
        self.position += 1;
        Ok(match token {
            Token::Integer(value) | Token::Character(value) => Expression::Integer(value),
            Token::Float(value) => Expression::Float(value.to_bits()),
            Token::Name(name) => match name.as_str() {
                "true" => Expression::Boolean(true),
                "false" => Expression::Boolean(false),
                "NULL" | "nullptr" | "nil" => Expression::Integer(0),
                _ => Expression::Path {
                    root: name,
                    selectors: Vec::new(),
                },
            },
            Token::Symbol("(") => {
                let inner = self.nested(Self::or)?;
                self.expect(")")?;
                inner
            }
            Token::Symbol(symbol) => {
                return Err(invalid(format!("unexpected '{symbol}' in the condition")));
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Evaluates with every path resolved from a table of names.
    fn check(text: &str, values: &[(&str, Operand)]) -> std::result::Result<bool, String> {
        let condition = Condition::parse(text).map_err(|error| error.to_string())?;
        condition.evaluate(&mut |path| {
            let name = path.to_string();
            values
                .iter()
                .find(|(candidate, _)| *candidate == name)
                .map(|(_, value)| *value)
                .ok_or_else(|| format!("no value {name}"))
        })
    }

    #[test]
    fn conditions_compare_compute_and_short_circuit() {
        let values = [
            ("x", Operand::Integer(5)),
            ("y", Operand::Float(2.5)),
            ("flag", Operand::Boolean(true)),
            ("(*p).next", Operand::Integer(0)),
            ("items[4]", Operand::Integer(9)),
            ("c", Operand::Integer(97)),
        ];
        for (text, expected) in [
            ("x == 5", true),
            ("x != 5", false),
            ("x > 3 && x <= 5", true),
            ("x < 3 || flag", true),
            ("!flag", false),
            ("x * 2 + 1 == 11", true),
            ("x % 2 == 1", true),
            ("-x < 0", true),
            ("y > 2", true),
            ("x + y == 7.5", true),
            ("p->next == NULL", true),
            ("items[x - 1] == 9", true),
            ("c == 'a'", true),
            ("x == 0x5", true),
            ("x", true),
            ("(x - 5)", false),
            // The right side is never needed, so its unknown value is fine.
            ("x == 5 || missing", true),
            ("x == 4 && missing", false),
        ] {
            assert_eq!(check(text, &values), Ok(expected), "{text}");
        }
        assert_eq!(
            check("missing > 1", &values),
            Err("no value missing".to_owned())
        );
        assert_eq!(
            check("x / 0", &values),
            Err("the condition divides by zero".to_owned())
        );
    }

    #[test]
    fn malformed_conditions_explain_themselves() {
        for (text, expected) in [
            ("", "a condition must not be empty"),
            ("x = 3", "'=' assigns; compare with '=='"),
            (
                "x & 1",
                "'&' is not supported; conditions use '&&' and '||'",
            ),
            (
                "1 < x < 3",
                "comparisons cannot be chained; join them with '&&'",
            ),
            ("(x", "expected ')' at the end"),
            ("x.", "'.' must be followed by a member name"),
            ("3.x", "'3.x' is not a number"),
            ("x y", "unexpected 'y' in the condition"),
            ("*3", "only a value can be dereferenced"),
            ("'ab'", "a character literal is malformed"),
            ("x # 1", "unexpected '#' in the condition"),
        ] {
            assert_eq!(
                Condition::parse(text)
                    .map(|_| ())
                    .map_err(|error| error.to_string()),
                Err(format!("invalid condition: {expected}")),
                "{text}"
            );
        }
        let deep = format!("{}x{}", "(".repeat(70), ")".repeat(70));
        assert!(Condition::parse(&deep).is_err());
    }

    #[test]
    fn log_messages_interpolate_paths_and_escape_braces() {
        let message = LogMessage::parse("x = {x}, next = {p->next} {{literal}}").expect("message");
        let segments = message
            .segments()
            .iter()
            .map(|segment| match segment {
                LogSegment::Text(text) => format!("text {text}"),
                LogSegment::Value(path) => format!("value {path}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            segments,
            [
                "text x = ",
                "value x",
                "text , next = ",
                "value (*p).next",
                "text  {literal}"
            ]
        );
        for (text, expected) in [
            ("{x", "a '{' has no '}'; write '{{' for a brace"),
            ("x}", "a '}' has no '{'; write '}}' for a brace"),
            (
                "{a + 1}",
                "'{a + 1}' is not a value path such as name, a.b, p->next, or items[2]",
            ),
            (
                "{a[i]}",
                "'{a[i]}' is not a value path such as name, a.b, p->next, or items[2]",
            ),
        ] {
            assert_eq!(
                LogMessage::parse(text)
                    .map(|_| ())
                    .map_err(|error| error.to_string()),
                Err(format!("invalid log message: {expected}"))
            );
        }
    }
}
