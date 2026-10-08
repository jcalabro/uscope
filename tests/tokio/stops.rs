//! What a test reads at a stop of an async Rust program.

use uscope::{Backtrace, Evaluation, Expression, ScalarValue, VariableState, VariableValue};

use crate::support::Scenario;

/// The fixture's source file `path`, under tests/fixtures/rust/tokio.
pub fn source(path: &str) -> String {
    format!("tests/fixtures/rust/tokio/{path}")
}

/// The line of `path` holding `marker`.
pub fn line(path: &str, marker: &str) -> u64 {
    crate::support::source_line(&source(path), marker)
}

/// An integer expression's value at the stop, or `None` where it is
/// unavailable.
pub async fn integer(scenario: &Scenario, text: &str) -> Option<i128> {
    let expression = Expression::parse(text).expect("an expression");
    let Evaluation::Value { value, .. } = scenario
        .operation(text, scenario.handle().evaluate(&expression))
        .await
    else {
        panic!("{text}: not a value");
    };
    match value.state {
        VariableState::Available {
            value: VariableValue::Scalar(ScalarValue::Signed(value)),
            ..
        } => Some(value),
        VariableState::Available {
            value: VariableValue::Scalar(ScalarValue::Unsigned(value)),
            ..
        } => Some(i128::try_from(value).expect("a small integer")),
        VariableState::Unavailable(_) => None,
        other => panic!("{text}: {other:?}"),
    }
}

/// The function and line the stop is at.
pub async fn place(scenario: &Scenario) -> (String, u64) {
    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;
    (
        location
            .image
            .function
            .map(|function| function.name.to_string())
            .unwrap_or_default(),
        location.image.source.map_or(0, |source| source.line.get()),
    )
}

/// The selected thread's or task's backtrace.
pub async fn backtrace(scenario: &Scenario) -> Backtrace {
    scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await
}

/// Each frame's function and line, innermost first, up to the first frame
/// named `last`.
pub fn frames_to(trace: &Backtrace, last: &str) -> Vec<(String, u64)> {
    let mut frames = Vec::new();
    for frame in trace.frames.iter() {
        let name = frame
            .function
            .as_ref()
            .map(|function| function.name.to_string())
            .unwrap_or_default();
        let line = frame.source.as_ref().map_or(0, |source| source.line.get());
        let done = name == last;
        frames.push((name, line));
        if done {
            break;
        }
    }
    frames
}
