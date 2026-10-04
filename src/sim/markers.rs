//! Markers: conditions a golden program's variables satisfy whenever the
//! line that carries them is about to run, written in the source as
//!
//! ```c
//! total += square(index); // MARK: total == (index - 1) * index * (2 * index - 1) / 6
//! ```
//!
//! A condition joins comparisons with `&&`; each compares two expressions
//! of integers, the variables in scope, `+`, `-`, `*`, `/`, `%`, and
//! parentheses. The variables oracle reads the variables from the debugger
//! at a stop there and requires the condition to hold.

use std::collections::BTreeMap;
use std::fmt;

/// A condition on one line of a program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub line: u64,
    pub condition: Condition,
    /// The condition as written.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition(Vec<Comparison>);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Comparison {
    left: Expression,
    operator: Operator,
    right: Expression,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operator {
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Expression {
    Number(i128),
    Variable(String),
    Binary(Box<Self>, char, Box<Self>),
}

/// What evaluating a condition found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Holds,
    /// A comparison failed, shown with the values it compared.
    Fails(String),
    /// A variable it needs has no value.
    Unknown(String),
}

impl Condition {
    /// Every variable the condition reads.
    #[must_use]
    pub fn variables(&self) -> Vec<&str> {
        let mut names = Vec::new();
        for comparison in &self.0 {
            comparison.left.variables(&mut names);
            comparison.right.variables(&mut names);
        }
        names.sort_unstable();
        names.dedup();
        names
    }

    /// Evaluates the condition with `values`.
    #[must_use]
    pub fn evaluate(&self, values: &BTreeMap<String, i128>) -> Verdict {
        for comparison in &self.0 {
            let (left, right) = match (
                comparison.left.evaluate(values),
                comparison.right.evaluate(values),
            ) {
                (Ok(left), Ok(right)) => (left, right),
                (Err(missing), _) | (_, Err(missing)) => return Verdict::Unknown(missing),
            };
            let holds = match comparison.operator {
                Operator::Equal => left == right,
                Operator::NotEqual => left != right,
                Operator::Less => left < right,
                Operator::LessOrEqual => left <= right,
                Operator::Greater => left > right,
                Operator::GreaterOrEqual => left >= right,
            };
            if !holds {
                return Verdict::Fails(format!(
                    "{} is {left} and {} is {right}",
                    comparison.left, comparison.right
                ));
            }
        }
        Verdict::Holds
    }
}

impl Expression {
    fn variables<'a>(&'a self, names: &mut Vec<&'a str>) {
        match self {
            Self::Number(_) => {}
            Self::Variable(name) => names.push(name),
            Self::Binary(left, _, right) => {
                left.variables(names);
                right.variables(names);
            }
        }
    }

    fn evaluate(&self, values: &BTreeMap<String, i128>) -> Result<i128, String> {
        match self {
            Self::Number(value) => Ok(*value),
            Self::Variable(name) => values.get(name).copied().ok_or_else(|| name.clone()),
            Self::Binary(left, operator, right) => {
                let (left, right) = (left.evaluate(values)?, right.evaluate(values)?);
                Ok(match operator {
                    '+' => left.wrapping_add(right),
                    '-' => left.wrapping_sub(right),
                    '*' => left.wrapping_mul(right),
                    '/' => left.checked_div(right).unwrap_or(0),
                    _ => left.checked_rem(right).unwrap_or(0),
                })
            }
        }
    }
}

impl fmt::Display for Expression {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(value) => write!(formatter, "{value}"),
            Self::Variable(name) => formatter.write_str(name),
            Self::Binary(left, operator, right) => {
                write!(formatter, "({left} {operator} {right})")
            }
        }
    }
}

/// Finds the markers in a program's source.
pub fn parse(source: &str) -> Result<Vec<Marker>, String> {
    source
        .lines()
        .zip(1..)
        .filter_map(|(text, line)| Some((text.split_once("// MARK:")?.1.trim(), line)))
        .map(|(text, line)| {
            Ok(Marker {
                line,
                condition: Parser::new(text)
                    .condition()
                    .map_err(|error| format!("line {line}: {error} in {text:?}"))?,
                text: text.to_owned(),
            })
        })
        .collect()
}

struct Parser<'a> {
    rest: &'a str,
}

impl<'a> Parser<'a> {
    const fn new(text: &'a str) -> Self {
        Self { rest: text }
    }

    fn skip_space(&mut self) {
        self.rest = self.rest.trim_start();
    }

    fn eat(&mut self, token: &str) -> bool {
        self.skip_space();
        match self.rest.strip_prefix(token) {
            Some(rest) => {
                self.rest = rest;
                true
            }
            None => false,
        }
    }

    fn condition(&mut self) -> Result<Condition, String> {
        let mut comparisons = vec![self.comparison()?];
        while self.eat("&&") {
            comparisons.push(self.comparison()?);
        }
        self.skip_space();
        if !self.rest.is_empty() {
            return Err(format!("unexpected {:?}", self.rest));
        }
        Ok(Condition(comparisons))
    }

    fn comparison(&mut self) -> Result<Comparison, String> {
        let left = self.sum()?;
        let operator = [
            ("==", Operator::Equal),
            ("!=", Operator::NotEqual),
            ("<=", Operator::LessOrEqual),
            (">=", Operator::GreaterOrEqual),
            ("<", Operator::Less),
            (">", Operator::Greater),
        ]
        .into_iter()
        .find_map(|(token, operator)| self.eat(token).then_some(operator))
        .ok_or("a comparison needs an operator")?;
        Ok(Comparison {
            left,
            operator,
            right: self.sum()?,
        })
    }

    fn sum(&mut self) -> Result<Expression, String> {
        let mut left = self.product()?;
        loop {
            let operator = if self.eat("+") {
                '+'
            } else if self.eat("-") {
                '-'
            } else {
                return Ok(left);
            };
            left = Expression::Binary(Box::new(left), operator, Box::new(self.product()?));
        }
    }

    fn product(&mut self) -> Result<Expression, String> {
        let mut left = self.atom()?;
        loop {
            let operator = if self.eat("*") {
                '*'
            } else if self.eat("/") {
                '/'
            } else if self.eat("%") {
                '%'
            } else {
                return Ok(left);
            };
            left = Expression::Binary(Box::new(left), operator, Box::new(self.atom()?));
        }
    }

    fn atom(&mut self) -> Result<Expression, String> {
        if self.eat("(") {
            let inner = self.sum()?;
            if !self.eat(")") {
                return Err("a parenthesis is not closed".into());
            }
            return Ok(inner);
        }
        self.skip_space();
        let length = self
            .rest
            .find(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .unwrap_or(self.rest.len());
        let (token, rest) = self.rest.split_at(length);
        self.rest = rest;
        if token.is_empty() {
            return Err("an operand is missing".into());
        }
        if token.starts_with(|character: char| character.is_ascii_digit()) {
            return token
                .parse()
                .map(Expression::Number)
                .map_err(|_| format!("{token} is not a number"));
        }
        Ok(Expression::Variable(token.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Markers parse from source comments, respect precedence, and say
    /// which comparison failed or which variable had no value.
    #[test]
    fn markers_evaluate_their_conditions() {
        let markers = parse(
            "int x;\n  total += square(index); // MARK: total == (index - 1) * index * (2 * index - 1) / 6 && index > 0\n",
        )
        .expect("a valid marker");
        assert_eq!(markers.len(), 1);
        let marker = &markers[0];
        assert_eq!(marker.line, 2);
        assert_eq!(marker.condition.variables(), ["index", "total"]);
        let values =
            |total, index| BTreeMap::from([("total".into(), total), ("index".into(), index)]);
        assert_eq!(marker.condition.evaluate(&values(14, 4)), Verdict::Holds);
        assert!(matches!(
            marker.condition.evaluate(&values(13, 4)),
            Verdict::Fails(message) if message.ends_with("is 13 and ((((index - 1) * index) * ((2 * index) - 1)) / 6) is 14")
        ));
        assert_eq!(
            marker.condition.evaluate(&values(0, 0)),
            Verdict::Fails("index is 0 and 0 is 0".into())
        );
        assert_eq!(
            marker
                .condition
                .evaluate(&BTreeMap::from([("total".into(), 0)])),
            Verdict::Unknown("index".into())
        );
        assert!(parse("// MARK: total ==").is_err());
        assert!(parse("// MARK: total").is_err());
        assert!(parse("// MARK: (a == 1").is_err());
    }
}
