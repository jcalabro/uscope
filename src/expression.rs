use crate::{
    Error, ParsedValueExpression, Result, ValueExpression, ValueIndexRange, ValuePathStep,
};

const MAX_EXPRESSION_BYTES: usize = 4096;
const MAX_EXPRESSION_STEPS: usize = 64;
const MAX_EXPRESSION_DEPTH: usize = 64;

/// Parses the debugger's bounded, source-neutral structural value syntax.
///
/// Supported operations are dotted names, integer array or slice indices,
/// explicit dereferences, parentheses, and one terminal half-open range.
pub fn parse_value_expression(input: &str) -> Result<ParsedValueExpression> {
    if input.is_empty() {
        return Err(invalid("an expression must name a data object"));
    }
    if input.len() > MAX_EXPRESSION_BYTES {
        return Err(invalid("the expression is too long"));
    }
    if input.chars().any(char::is_whitespace) {
        return Err(invalid("whitespace is not allowed in a value expression"));
    }
    if input.contains("->")
        || input.starts_with('&')
        || input.chars().all(|character| character.is_ascii_digit())
    {
        return Err(invalid("unsupported value-expression syntax"));
    }

    let mut parser = Parser {
        input,
        cursor: 0,
        steps: Vec::new(),
        range: None,
    };
    parser.parse_unary(0)?;
    if parser.cursor != input.len() {
        return Err(invalid("unexpected trailing expression syntax"));
    }
    if parser.steps.is_empty() || !matches!(parser.steps.first(), Some(ValuePathStep::Named(_))) {
        return Err(invalid("an expression must begin with a data-object name"));
    }
    if parser.steps.len() > MAX_EXPRESSION_STEPS {
        return Err(invalid("the expression contains too many operations"));
    }

    Ok(ParsedValueExpression {
        expression: ValueExpression {
            steps: parser.steps.into(),
        },
        range: parser.range,
    })
}

struct Parser<'input> {
    input: &'input str,
    cursor: usize,
    steps: Vec<ValuePathStep>,
    range: Option<ValueIndexRange>,
}

impl Parser<'_> {
    fn parse_unary(&mut self, depth: usize) -> Result<()> {
        if depth >= MAX_EXPRESSION_DEPTH {
            return Err(invalid("expression nesting exceeds its limit"));
        }
        if self.consume('*') {
            self.parse_unary(depth + 1)?;
            self.push(ValuePathStep::Dereference)?;
            return Ok(());
        }
        self.parse_primary(depth)
    }

    fn parse_primary(&mut self, depth: usize) -> Result<()> {
        if self.consume('(') {
            self.parse_unary(depth + 1)?;
            if !self.consume(')') {
                return Err(invalid("an opening parenthesis has no matching close"));
            }
        } else {
            let name = self.parse_name()?.to_owned();
            self.push(ValuePathStep::Named(name))?;
        }

        loop {
            if self.range.is_some() {
                break;
            }
            if self.consume('.') {
                let name = self.parse_name()?.to_owned();
                self.push(ValuePathStep::Named(name))?;
            } else if self.consume('[') {
                self.parse_subscript()?;
            } else {
                break;
            }
        }
        Ok(())
    }

    fn parse_subscript(&mut self) -> Result<()> {
        let start = self.cursor;
        let Some(relative_end) = self.input[start..].find(']') else {
            return Err(invalid("an array subscript has no closing bracket"));
        };
        let end = start + relative_end;
        let contents = &self.input[start..end];
        self.cursor = end + ']'.len_utf8();
        if contents.is_empty() {
            return Err(invalid("an array subscript must not be empty"));
        }
        if contents.contains('[') {
            return Err(invalid("nested array brackets are invalid"));
        }

        if let Some((start, end)) = contents.split_once("..") {
            if self.range.is_some() || start.is_empty() || end.is_empty() || end.contains("..") {
                return Err(invalid("a range must have exactly one start and end"));
            }
            self.range = Some(ValueIndexRange {
                start: parse_index(start)?,
                end: parse_index(end)?,
            });
        } else {
            self.push(ValuePathStep::Index(parse_index(contents)?))?;
        }
        Ok(())
    }

    fn parse_name(&mut self) -> Result<&str> {
        let start = self.cursor;
        let end = self.input[start..]
            .char_indices()
            .find_map(|(offset, character)| {
                matches!(character, '.' | '[' | ']' | '(' | ')' | '*').then_some(start + offset)
            })
            .unwrap_or(self.input.len());
        if start == end {
            return Err(invalid("expression names must not be empty"));
        }
        self.cursor = end;
        Ok(&self.input[start..end])
    }

    fn consume(&mut self, expected: char) -> bool {
        if self.input[self.cursor..].starts_with(expected) {
            self.cursor += expected.len_utf8();
            true
        } else {
            false
        }
    }

    fn push(&mut self, step: ValuePathStep) -> Result<()> {
        if self.steps.len() >= MAX_EXPRESSION_STEPS {
            return Err(invalid("the expression contains too many operations"));
        }
        self.steps.push(step);
        Ok(())
    }
}

fn parse_index(input: &str) -> Result<i128> {
    input
        .parse()
        .map_err(|_| invalid("an array index must be a signed 128-bit integer"))
}

fn invalid(description: impl Into<String>) -> Error {
    Error::InvalidValueExpression(description.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> ValuePathStep {
        ValuePathStep::Named(name.to_owned())
    }

    #[test]
    fn parses_ordered_members_indices_dereferences_and_ranges() {
        let parsed =
            parse_value_expression("records[1].values[1]").expect("indexed member expression");
        assert_eq!(
            parsed.expression.steps.as_ref(),
            [
                named("records"),
                ValuePathStep::Index(1),
                named("values"),
                ValuePathStep::Index(1),
            ]
        );
        assert_eq!(parsed.range, None);

        let dereferenced =
            parse_value_expression("(*records)[1].values[1]").expect("explicit dereference");
        assert_eq!(
            dereferenced.expression.steps.as_ref(),
            [
                named("records"),
                ValuePathStep::Dereference,
                ValuePathStep::Index(1),
                named("values"),
                ValuePathStep::Index(1),
            ]
        );

        let range = parse_value_expression("huge_array[0..10]").expect("terminal range");
        assert_eq!(range.expression.steps.as_ref(), [named("huge_array")]);
        assert_eq!(range.range, Some(ValueIndexRange { start: 0, end: 10 }));
    }

    #[test]
    fn preserves_qualified_dotted_roots_and_existing_dereference_precedence() {
        let parsed =
            parse_value_expression("**one.c::duplicate.member").expect("qualified expression");
        assert_eq!(
            parsed.expression.steps.as_ref(),
            [
                named("one"),
                named("c::duplicate"),
                named("member"),
                ValuePathStep::Dereference,
                ValuePathStep::Dereference,
            ]
        );

        let package =
            parse_value_expression("github.com/acme/my-pkg.global").expect("package global");
        assert_eq!(
            package.expression.steps.as_ref(),
            [named("github"), named("com/acme/my-pkg"), named("global")]
        );
    }

    #[test]
    fn rejects_malformed_or_unbounded_syntax() {
        for input in [
            "",
            "*",
            ".pair",
            "pair.",
            "pair..first",
            "pair[]",
            "pair[abc]",
            "pair[0",
            "pair[0..]",
            "pair[..1]",
            "pair[0...1]",
            "pair[0..1].field",
            "pair[0..1][2]",
            "(pair",
            "pair)",
            "pair  .field",
            "42",
        ] {
            assert!(
                parse_value_expression(input).is_err(),
                "accepted malformed expression {input:?}"
            );
        }

        let deeply_nested = format!("{}value{}", "(".repeat(65), ")".repeat(65));
        assert!(parse_value_expression(&deeply_nested).is_err());
        assert!(parse_value_expression(&"x".repeat(MAX_EXPRESSION_BYTES + 1)).is_err());
    }
}
