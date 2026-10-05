//! Runs every example in the language's reference, `docs/expressions.md`,
//! so that the reference and the implementation cannot drift apart.

use super::syntax::Expression;

const REFERENCE: &str = include_str!("../../docs/expressions.md");

/// One `expression => outcome` row of an example block.
struct Row<'text> {
    line: usize,
    expression: &'text str,
    outcome: &'text str,
}

fn rows() -> Vec<Row<'static>> {
    let mut rows = Vec::new();
    let mut in_block = false;
    for (index, line) in REFERENCE.lines().enumerate() {
        match line.trim() {
            "```uscope-example" => in_block = true,
            "```" => in_block = false,
            "" => {}
            row if in_block => {
                let (expression, outcome) = row
                    .split_once(" => ")
                    .unwrap_or_else(|| panic!("line {}: `{row}` has no ` => `", index + 1));
                rows.push(Row {
                    line: index + 1,
                    expression: expression.trim(),
                    outcome: outcome.trim(),
                });
            }
            _ => {}
        }
    }
    rows
}

/// The text of a Markdown code span, in single or double backticks.
fn code(span: &str) -> Option<&str> {
    span.strip_prefix("`` ")
        .and_then(|inner| inner.strip_suffix(" ``"))
        .or_else(|| span.strip_prefix('`')?.strip_suffix('`'))
}

#[test]
fn every_example_in_the_reference_holds() {
    let rows = rows();
    assert!(rows.len() > 50, "the reference's examples were found");
    for row in rows {
        let context = format!("docs/expressions.md:{}: `{}`", row.line, row.expression);
        let parsed = Expression::parse(row.expression);
        if let Some(normal) = row.outcome.strip_prefix("reads as ") {
            let normal = code(normal).unwrap_or_else(|| panic!("{context}: malformed outcome"));
            let expression = parsed.unwrap_or_else(|error| panic!("{context}: {error}"));
            assert_eq!(expression.to_string(), normal, "{context}");
        } else if let Some(error) = row.outcome.strip_prefix("error ") {
            let (kind, pointed) = error
                .split_once(" at ")
                .unwrap_or_else(|| panic!("{context}: malformed error outcome"));
            let pointed = code(pointed).unwrap_or_else(|| panic!("{context}: malformed span"));
            let Err(error) = parsed else {
                panic!("{context}: parsed, but the reference expects an error");
            };
            assert_eq!(error.kind.name(), kind, "{context}: {error}");
            assert_eq!(
                error.span.text(row.expression),
                pointed,
                "{context}: {error}"
            );
        } else {
            panic!("{context}: unknown outcome `{}`", row.outcome);
        }
    }
}

/// The evaluator is pure: it reaches a program only through the traits the
/// debugger implements for it, so none of its code may reach for process
/// control, debug information, I/O, clocks, or threads. Tests are exempt.
#[test]
fn the_evaluator_stays_pure() {
    const FORBIDDEN: [&str; 11] = [
        "crate::backend",
        "crate::debug_info",
        "crate::sim",
        "nix::",
        "gimli",
        "tokio",
        "std::fs",
        "std::env",
        "std::process",
        "std::thread",
        "std::time",
    ];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/eval");
    let mut pending = vec![root];
    let mut checked = 0;
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            let entries = std::fs::read_dir(&path).expect("read the evaluator's sources");
            pending.extend(entries.map(|entry| entry.expect("a directory entry").path()));
            continue;
        }
        let test_file = path.file_name().is_some_and(|name| name == "tests.rs")
            || path.components().any(|part| part.as_os_str() == "tests");
        if test_file || path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read a source file");
        // Code compiled only for tests or the fuzzer may use anything.
        let source = source.split("#[cfg(test)]").next().unwrap_or_default();
        for forbidden in FORBIDDEN {
            assert!(
                !source.contains(forbidden),
                "{} uses `{forbidden}`",
                path.display()
            );
        }
        checked += 1;
    }
    assert!(checked >= 6, "the evaluator's sources were found");
}
