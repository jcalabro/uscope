//! Expressions evaluated at real stops agree with what the program itself
//! computes. Each fixture prints `EXPECT` lines before it calls `barrier()`,
//! and every expression is evaluated from `barrier`'s caller.

use std::process::Stdio;

use uscope::{Evaluation, Expression, FloatValue, IntegerValue, StackFrameId, VariableValue};

use super::*;
use crate::support::ScratchDir;

/// One line a fixture printed: an expression and what it must evaluate to.
struct Expectation {
    expression: String,
    kind: String,
    value: String,
}

fn expectations(output: &str) -> Vec<Expectation> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.strip_prefix("EXPECT\t")?.split('\t');
            Some(Expectation {
                expression: fields.next()?.to_owned(),
                kind: fields.next()?.to_owned(),
                value: fields.next()?.to_owned(),
            })
        })
        .collect()
}

/// A value as a fixture prints it, for the kind it printed.
fn rendered(kind: &str, value: &VariableValue) -> Option<String> {
    let integer = |value: &IntegerValue| match value {
        IntegerValue::Signed(value) => value.to_string(),
        IntegerValue::Unsigned(value) => value.to_string(),
        _ => unreachable!("integer values are signed or unsigned"),
    };
    Some(match (kind, value) {
        ("int", VariableValue::Scalar(ScalarValue::Signed(value))) => value.to_string(),
        ("int", VariableValue::Scalar(ScalarValue::Unsigned(value))) => value.to_string(),
        ("int", VariableValue::Enumeration { value, .. }) => integer(value),
        ("bool", VariableValue::Scalar(ScalarValue::Boolean(value))) => value.to_string(),
        ("address", VariableValue::Address(address)) => format!("{:#x}", address.address.get()),
        ("f32", VariableValue::Scalar(ScalarValue::Floating(FloatValue::Binary32(bits)))) => {
            format!("{bits:#x}")
        }
        ("f64", VariableValue::Scalar(ScalarValue::Floating(FloatValue::Binary64(bits)))) => {
            format!("{bits:#x}")
        }
        (
            "f80",
            VariableValue::Scalar(ScalarValue::Floating(FloatValue::X87Extended {
                significand,
                sign_exponent,
            })),
        ) => format!("{sign_exponent:#x}:{significand:#x}"),
        _ => return None,
    })
}

/// Runs a fixture to `barrier`, selects its caller, and checks every
/// expectation. In optimized builds a row may be explicitly unavailable;
/// a different value always fails.
async fn check_fixture(fixture: &str, optimized: bool) {
    let scratch = ScratchDir::new("expressions");
    let output_path = scratch.path().join("stdout");
    let output = std::fs::File::create(&output_path).expect("create the fixture's output");
    let mut scenario = Scenario::launch(fixture);
    scenario.add_breakpoint("barrier").await;
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            stdout: Some(Stdio::from(output)),
            ..LaunchOptions::default()
        })
        .await;
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{fixture}: {reason:?}"
    );
    // The fixture flushes its expectations before it calls barrier.
    let printed = std::fs::read_to_string(&output_path).expect("read the fixture's output");
    let expectations = expectations(&printed);
    assert!(
        expectations.len() > 40,
        "{fixture} printed its expectations: {printed}"
    );

    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let caller: StackFrameId = trace.frames[1].id;
    scenario
        .operation("select caller", scenario.handle().select_frame(caller))
        .await;

    let mut failures = Vec::new();
    for expectation in &expectations {
        let expression = match Expression::parse(&expectation.expression) {
            Ok(expression) => expression,
            Err(error) => {
                failures.push(format!("`{}`: {error}", expectation.expression));
                continue;
            }
        };
        let result = scenario.handle().evaluate(&expression).await;
        let message = match result {
            Ok(Evaluation::Value { value, cause }) => match &value.state {
                VariableState::Available { value: decoded, .. } => {
                    match rendered(&expectation.kind, decoded) {
                        Some(actual) if actual == expectation.value => continue,
                        actual => format!(
                            "{} ({}) instead of {}",
                            actual.unwrap_or_else(|| format!("{decoded:?}")),
                            value
                                .type_info
                                .map(|info| info.name)
                                .as_deref()
                                .unwrap_or("?"),
                            expectation.value
                        ),
                    }
                }
                VariableState::Unavailable(_) if optimized && cause.is_some() => continue,
                state => format!("{state:?} at {cause:?}"),
            },
            other => format!("{other:?}"),
        };
        failures.push(format!("`{}`: {message}", expectation.expression));
    }
    assert!(failures.is_empty(), "{fixture}:\n{}", failures.join("\n"));
    scenario.shutdown().await;
}

#[tokio::test]
async fn c_expressions_agree_with_the_program_across_the_compiler_matrix() {
    for compiler in ["gcc", "clang"] {
        for optimization in ["o0", "o2"] {
            for linking in ["pie", "nopie"] {
                let fixture = format!("expressions-c-{compiler}-{optimization}-{linking}");
                check_fixture(&fixture, optimization == "o2").await;
            }
        }
    }
}
