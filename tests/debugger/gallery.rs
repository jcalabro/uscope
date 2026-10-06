//! Values agree with what their programs say they are. Each gallery prints
//! a tab-separated `TRUTH` line per value before it calls `reached()`, and
//! the values are read from `reached`'s caller, or from the stopped frame
//! at another breakpoint. A value uscope shows as available must equal the
//! truth; in optimized builds it may instead be unavailable for a typed
//! reason, unless the test requires it.

use std::process::Stdio;

use uscope::{
    FloatValue, IntegerValue, LaunchOptions, StackFrameId, TypeInfo, ValueChildRelationship,
    Variable, VariableKind, VariableValue, VariableValueSource,
};

use super::*;
use crate::support::ScratchDir;

/// One line a gallery printed: what one value at one checkpoint is.
#[derive(Debug, Clone)]
struct Truth {
    checkpoint: String,
    path: String,
    kind: String,
    value: String,
}

fn truths(output: &str) -> Vec<Truth> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.strip_prefix("TRUTH\t")?.split('\t');
            Some(Truth {
                checkpoint: fields.next()?.to_owned(),
                path: fields.next()?.to_owned(),
                kind: fields.next()?.to_owned(),
                value: fields.next().unwrap_or_default().to_owned(),
            })
        })
        .collect()
}

/// How one gallery build runs and what it must show.
struct Gallery<'a> {
    fixture: &'a str,
    /// The functions to stop in: `reached`, whose caller is inspected, and
    /// others inspected where they stop.
    breakpoints: &'a [&'a str],
    /// Checkpoints the run must reach, in order.
    checkpoints: &'a [&'a str],
    optimized: bool,
    /// `checkpoint:path` values an optimized build must show all the same.
    required: &'a [&'a str],
    /// Whether every listed variable must have a name its program wrote.
    go: bool,
}

/// Runs a gallery through every checkpoint and checks every truth it
/// printed against what uscope shows there.
async fn check_gallery(gallery: &Gallery<'_>) {
    let fixture = gallery.fixture;
    let scratch = ScratchDir::new("gallery");
    let output_path = scratch.path().join("stdout");
    let output = std::fs::File::create(&output_path).expect("create the gallery's output");
    let mut scenario = Scenario::launch(fixture);
    for breakpoint in gallery.breakpoints {
        scenario.add_breakpoint(breakpoint).await;
    }
    let mut reason = scenario
        .run_with_to_stop(LaunchOptions {
            stdout: Some(Stdio::from(output)),
            ..LaunchOptions::default()
        })
        .await;
    let mut visited = Vec::new();
    let mut failures = Vec::new();
    loop {
        match reason {
            StopReason::Breakpoint { .. } => {}
            // Go preempts its threads with SIGURG, which never stops a
            // program on its own.
            StopReason::Exception(ref exception) if exception.code == 23 => {
                reason = scenario.resume_to_stop().await;
                continue;
            }
            StopReason::Exited(ExitStatus::Code(0)) => break,
            other => panic!("{fixture} stopped unexpectedly: {other:?}"),
        }
        let printed = std::fs::read_to_string(&output_path).expect("read the gallery's output");
        let truths = truths(&printed);
        let checkpoint = truths
            .last()
            .unwrap_or_else(|| panic!("{fixture} stopped before printing any truth"))
            .checkpoint
            .clone();
        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        let innermost = trace.frames[0]
            .function
            .as_ref()
            .map(|function| function.name.to_string())
            .unwrap_or_default();
        if innermost.ends_with("reached") {
            let caller: StackFrameId = trace.frames[1].id;
            scenario
                .operation("select caller", scenario.handle().select_frame(caller))
                .await;
        }
        let snapshot = scenario
            .operation("variables", scenario.handle().variables())
            .await;
        if gallery.go {
            for variable in snapshot.variables.iter() {
                if variable.name.starts_with(['.', '#', '&']) {
                    failures.push(format!(
                        "{checkpoint}: lists the compiler's {}",
                        variable.name
                    ));
                }
            }
        }
        for truth in truths.iter().filter(|truth| truth.checkpoint == checkpoint) {
            let required = gallery
                .required
                .contains(&format!("{checkpoint}:{}", truth.path).as_str());
            if let Err(failure) = check_truth(
                &scenario,
                &snapshot.variables,
                truth,
                gallery.optimized && !required,
            )
            .await
            {
                failures.push(format!("{checkpoint}: {}: {failure}", truth.path));
            }
        }
        visited.push(checkpoint);
        reason = scenario.resume_to_stop().await;
    }
    assert!(failures.is_empty(), "{fixture}:\n{}", failures.join("\n"));
    assert_eq!(visited, gallery.checkpoints, "{fixture}");
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

/// Checks one truth, accepting a typed unavailable or malformed state
/// when `may_be_unavailable`.
async fn check_truth(
    scenario: &Scenario,
    variables: &[Variable],
    truth: &Truth,
    may_be_unavailable: bool,
) -> Result<(), String> {
    let mut segments = truth.path.split('.');
    let name = segments.next().expect("a path names a variable");
    let variable = variables
        .iter()
        .rev()
        .find(|variable| variable.name.as_ref() == name);
    if truth.kind == "absent" {
        return variable.map_or(Ok(()), |variable| Err(format!("is listed: {variable:?}")));
    }
    let variable = variable.ok_or_else(|| "is not listed".to_owned())?;
    if truth.kind == "result" {
        return if variable.kind == VariableKind::Result {
            Ok(())
        } else {
            Err(format!("is listed as a {:?}", variable.kind))
        };
    }
    let mut type_info = variable.type_info.clone();
    let mut type_name = variable.type_info.as_ref().map(|info| info.name.clone());
    let mut state = variable.state.clone();
    for segment in segments {
        let VariableState::Available {
            children: uscope::ValueChildren::Available(reference),
            ..
        } = &state
        else {
            break;
        };
        let page = child_page(scenario, &state, 0, 256).await;
        let child = page
            .children
            .iter()
            .find(|child| match &child.relationship {
                ValueChildRelationship::Member(member) => member.name.as_deref() == Some(segment),
                ValueChildRelationship::SliceElement { index }
                | ValueChildRelationship::ArrayElement { index, .. } => {
                    index.to_string() == segment
                }
                _ => false,
            })
            .ok_or_else(|| {
                format!(
                    "has no child {segment} among {} children",
                    reference.total()
                )
            });
        // Optimized code may leave out what nothing reads, such as a
        // variable a closure captured.
        let child = match child {
            Ok(child) => child,
            Err(_) if may_be_unavailable => return Ok(()),
            Err(failure) => return Err(failure),
        };
        type_name = Some(child.type_info.name.clone());
        type_info = Some(child.type_info.clone());
        state = child.state.clone();
    }
    let VariableState::Available { .. } = &state else {
        return if may_be_unavailable
            && matches!(
                state,
                VariableState::Unavailable(_) | VariableState::Malformed(_)
            ) {
            Ok(())
        } else {
            Err(format!("is not available: {state:?}"))
        };
    };
    let shown = shown(&truth.kind, &state, type_info.as_ref());
    if shown == truth.value {
        Ok(())
    } else {
        Err(format!(
            "shows {shown} ({}) instead of {} {}",
            type_name.as_deref().unwrap_or("?"),
            truth.kind,
            truth.value
        ))
    }
}

/// What uscope shows of an available value, written as a truth of `kind`
/// writes it.
fn shown(kind: &str, state: &VariableState, type_info: Option<&TypeInfo>) -> String {
    let VariableState::Available { value, text, .. } = state else {
        unreachable!("only available values are shown");
    };
    let integer = |value: &IntegerValue| match value {
        IntegerValue::Signed(value) => value.to_string(),
        IntegerValue::Unsigned(value) => value.to_string(),
        _ => unreachable!("integers are signed or unsigned"),
    };
    match (kind, value) {
        ("int" | "uint", VariableValue::Scalar(ScalarValue::Signed(value))) => value.to_string(),
        ("int" | "uint", VariableValue::Scalar(ScalarValue::Unsigned(value))) => value.to_string(),
        ("int" | "uint", VariableValue::Enumeration { value, .. }) => integer(value),
        ("f32", VariableValue::Scalar(ScalarValue::Floating(FloatValue::Binary32(bits)))) => {
            format!("{bits:#x}")
        }
        ("f64", VariableValue::Scalar(ScalarValue::Floating(FloatValue::Binary64(bits)))) => {
            format!("{bits:#x}")
        }
        (
            "c64",
            VariableValue::Scalar(ScalarValue::Complex {
                real: FloatValue::Binary32(real),
                imaginary: FloatValue::Binary32(imaginary),
            }),
        ) => format!("{real:#x}:{imaginary:#x}"),
        (
            "c128",
            VariableValue::Scalar(ScalarValue::Complex {
                real: FloatValue::Binary64(real),
                imaginary: FloatValue::Binary64(imaginary),
            }),
        ) => format!("{real:#x}:{imaginary:#x}"),
        ("summary", _) => uscope::value_summary(type_info, state),
        ("addressable", _) => match state {
            VariableState::Available {
                source: VariableValueSource::Memory(_),
                ..
            } => String::new(),
            VariableState::Available { source, .. } => format!("{source:?}"),
            _ => unreachable!("only available values are shown"),
        },
        ("func", VariableValue::Function { code, function }) => {
            uscope::function_text(*code, function.as_deref())
        }
        ("len", VariableValue::Slice { length, .. }) => length.to_string(),
        ("string", _) => text
            .as_ref()
            .map_or_else(|| format!("{value:?}"), |text| uscope::quoted_text(text)),
        ("type", _) => type_info.map_or("?", |info| &info.name).to_owned(),
        _ => format!("{value:?}"),
    }
}

#[tokio::test]
async fn c_pieces_agree_with_their_program() {
    for (fixture, optimized, required) in [
        ("pieces-gcc-o0", false, &[][..]),
        (
            "pieces-gcc-o2",
            true,
            &[
                "split:local.first",
                "complex:small",
                "complex:large",
                "complex:product",
            ][..],
        ),
        (
            "pieces-clang-o2",
            true,
            &[
                "split:local.first",
                "split:local.second",
                "split:pair.second",
                "complex:large",
                "complex:product",
            ][..],
        ),
    ] {
        check_gallery(&Gallery {
            fixture,
            breakpoints: &["reached"],
            checkpoints: &["split", "complex"],
            optimized,
            required,
            go: false,
        })
        .await;
    }
}

#[tokio::test]
async fn go_values_agree_with_their_program() {
    for (fixture, optimized, required) in [
        ("values-go-o0", false, &[][..]),
        (
            "values-go-o2",
            true,
            &[
                "pieces:text",
                "pieces:numbers",
                "pieces:numbers.1",
                "pieces:pair.X",
                "pieces:pair.Y",
                "pieces:ratio",
                "complex:small",
                "complex:large",
                "funcs:closure",
                "funcs:closure.offset",
                "funcs:closure.total",
                "escape:counter",
            ][..],
        ),
    ] {
        check_gallery(&Gallery {
            fixture,
            breakpoints: &["main.reached", "main.pieces"],
            checkpoints: &[
                "complex",
                "funcs",
                "escape",
                "results",
                "unnamed-results",
                "visibility-before",
                "visibility-after",
                "pieces",
            ],
            optimized,
            required,
            go: true,
        })
        .await;
    }
}
