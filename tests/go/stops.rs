//! What a test reads at a stop of a Go program.

use uscope::{Evaluation, Expression, ScalarValue, VariableState, VariableValue};

use crate::support::Scenario;

/// An integer expression's value at the stop, or `None` where the program's
/// debug information leaves it unavailable.
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

/// An address expression's value at the stop, where it is available.
pub async fn address(scenario: &Scenario, text: &str) -> Option<u64> {
    let expression = Expression::parse(text).expect("an expression");
    let Evaluation::Value { value, .. } = scenario
        .operation(text, scenario.handle().evaluate(&expression))
        .await
    else {
        panic!("{text}: not a value");
    };
    match value.state {
        VariableState::Available {
            value: VariableValue::Address(address),
            ..
        } => Some(address.address.get()),
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
