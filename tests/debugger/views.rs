//! The built-in views present the standard libraries' containers as the
//! `containers` fixtures' `VIEW:` markers say, in every build of the
//! matrix, and every built-in view binds in some build.
//!
//! A marker reads `VIEW: <expression> => <summary>`, where `{c*N}` stands
//! for N of the character c, or `VIEW: <expression> => problem: <words>`
//! when the view must refuse the value. Each expression is evaluated in
//! the frame that calls `barrier`.

use uscope::{
    Evaluation, Expression, InspectedValue, PresentedCount, PresentedShape, StackFrameId,
    ValueChildQuery, ValueChildRelationship, ValueChildren,
};

use super::*;

/// What a marker says its expression shows.
enum Expected {
    Summary(String),
    /// The view refuses the value, saying this.
    Problem(String),
}

struct Marker {
    line: usize,
    expression: String,
    expected: Expected,
}

/// `{c*N}` as N of the character c.
fn expand(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        let Some((character, count)) = rest[start + 1..]
            .split_once('}')
            .and_then(|(inner, _)| inner.split_once('*'))
            .and_then(|(character, count)| {
                Some((character.chars().next()?, count.parse::<usize>().ok()?))
            })
        else {
            out.push_str(&rest[..=start]);
            rest = &rest[start + 1..];
            continue;
        };
        out.push_str(&rest[..start]);
        out.extend(std::iter::repeat_n(character, count));
        rest = &rest[start + 1 + rest[start + 1..].find('}').expect("closed") + 1..];
    }
    out.push_str(rest);
    out
}

fn markers(source: &str) -> Vec<Marker> {
    let path = format!("{}/tests/fixtures/{source}", env!("CARGO_MANIFEST_DIR"));
    let text = fs::read_to_string(&path).expect("read the fixture's source");
    text.lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let marker = line.split_once("VIEW: ")?.1;
            let (expression, expected) = marker.split_once(" => ")?;
            Some(Marker {
                line: index + 1,
                expression: expression.trim().to_owned(),
                expected: expected.trim().strip_prefix("problem: ").map_or_else(
                    || Expected::Summary(expand(expected.trim())),
                    |problem| Expected::Problem(problem.to_owned()),
                ),
            })
        })
        .collect()
}

async fn evaluate(scenario: &Scenario, text: &str) -> InspectedValue {
    let expression = Expression::parse(text).unwrap_or_else(|error| panic!("`{text}`: {error}"));
    match scenario
        .operation(text, scenario.handle().evaluate(&expression))
        .await
    {
        Evaluation::Value { value, .. } => value,
        other => panic!("`{text}` is not a value: {other:?}"),
    }
}

fn presentation(value: &InspectedValue) -> Option<&uscope::Presentation> {
    match &value.state {
        VariableState::Available { presentation, .. } => presentation.as_deref(),
        _ => None,
    }
}

fn summary(value: &InspectedValue) -> String {
    match &value.state {
        VariableState::Available {
            presentation: Some(presentation),
            ..
        } => presentation.summary.to_string(),
        state => format!("{state:?}"),
    }
}

/// Checks a presented sequence's children: its elements, which evaluate
/// back by their index, the same in pages of any size; its fields; and
/// `[raw]`, the value as stored.
async fn check_children(
    scenario: &Scenario,
    marker: &Marker,
    presentation: &uscope::Presentation,
    failures: &mut Vec<String>,
) {
    let ValueChildren::Available(reference) = &presentation.children else {
        failures.push(format!("line {}: no children", marker.line));
        return;
    };
    let count = match presentation.count {
        Some(PresentedCount::Exact(count)) => count,
        _ => 0,
    };
    let page = |offset, limit| {
        scenario
            .handle()
            .value_children(Arc::clone(reference), ValueChildQuery { offset, limit })
    };
    // A whole page of elements needs more reads than the default budget.
    let whole = scenario
        .operation(
            "children",
            scenario.handle().value_children_with_limits(
                Arc::clone(reference),
                ValueChildQuery {
                    offset: 0,
                    limit: u32::try_from(reference.total().min(256)).expect("small"),
                },
                uscope::InspectionLimits {
                    memory_reads: 1024,
                    ..uscope::InspectionLimits::default()
                },
            ),
        )
        .await;
    let elements = whole
        .children
        .iter()
        .filter(|child| matches!(child.relationship, ValueChildRelationship::Element { .. }))
        .count() as u64;
    if elements != count.min(256) {
        failures.push(format!(
            "line {}: {elements} elements of {count}",
            marker.line
        ));
    }
    let raw = whole.children.last();
    if !matches!(
        raw,
        Some(uscope::ValueChild {
            relationship: ValueChildRelationship::Raw,
            state: VariableState::Available {
                presentation: None,
                ..
            },
            ..
        })
    ) && reference.total() <= 256
    {
        failures.push(format!("line {}: no [raw] child: {raw:?}", marker.line));
    }
    // Small pages read the same elements as one large page.
    let mut paged = Vec::new();
    let mut offset = 0;
    while offset < count.min(32) {
        let small = scenario.operation("a small page", page(offset, 7)).await;
        paged.extend(small.children.iter().map(|child| child.state.clone()));
        offset += 7;
    }
    let expected = whole
        .children
        .iter()
        .take(paged.len())
        .map(|child| child.state.clone())
        .collect::<Vec<_>>();
    if paged.len() > expected.len() || paged[..expected.len()] != expected[..] {
        failures.push(format!(
            "line {}: pages of 7 read other elements",
            marker.line
        ));
    }
    // An element's name evaluates back to it.
    for index in [0, count.saturating_sub(1)] {
        if index >= count.min(256) {
            continue;
        }
        let child = &whole.children[usize::try_from(index).expect("small")];
        let again = evaluate(scenario, &format!("({})[{index}]", marker.expression)).await;
        let rendered = |state: &VariableState| match state {
            VariableState::Available {
                presentation: Some(presentation),
                ..
            } => presentation.summary.to_string(),
            VariableState::Available { value, text, .. } => format!("{value:?} {text:?}"),
            state => format!("{state:?}"),
        };
        if rendered(&again.state) != rendered(&child.state) {
            failures.push(format!(
                "line {}: `{}[{index}]` is {} but its child is {}",
                marker.line,
                marker.expression,
                rendered(&again.state),
                rendered(&child.state)
            ));
        }
    }
}

/// Stops a containers build at `barrier`, checks every marker in its
/// caller, and returns the views that presented values.
async fn check_containers(
    fixture: &str,
    source: &str,
    barrier: &str,
    optimized: bool,
) -> BTreeSet<String> {
    let mut scenario = Scenario::launch(fixture);
    scenario.add_breakpoint(barrier).await;
    let reason = scenario.run_to_stop().await;
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{fixture}: {reason:?}"
    );
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let caller: StackFrameId = trace.frames[1].id;
    scenario
        .operation("select caller", scenario.handle().select_frame(caller))
        .await;

    let markers = markers(source);
    assert!(markers.len() >= 5, "{source} has its markers");
    let mut seen = BTreeSet::new();
    let mut failures = Vec::new();
    for marker in &markers {
        let value = evaluate(&scenario, &marker.expression).await;
        if optimized && matches!(value.state, VariableState::Unavailable(_)) {
            continue;
        }
        // Text a language's own types hold, such as Rust's `Box<str>`, needs
        // no view.
        if let (
            VariableState::Available {
                text: Some(text),
                presentation: None,
                ..
            },
            Expected::Summary(expected),
        ) = (&value.state, &marker.expected)
        {
            if &uscope::quoted_text(text) != expected {
                failures.push(format!("line {}: the text is {text:?}", marker.line));
            }
            continue;
        }
        let Some(presentation) = presentation(&value) else {
            failures.push(format!(
                "line {}: `{}` has no presentation: {:?}",
                marker.line, marker.expression, value.state
            ));
            continue;
        };
        seen.insert(presentation.view.to_string());
        match (&marker.expected, presentation.shape) {
            (Expected::Problem(words), PresentedShape::Raw) => {
                let problem = presentation
                    .problem
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                if !problem.contains(words.as_str()) {
                    failures.push(format!("line {}: the problem is `{problem}`", marker.line));
                }
            }
            (Expected::Problem(_), _) => failures.push(format!(
                "line {}: `{}` shows as {}, not a problem",
                marker.line,
                marker.expression,
                summary(&value)
            )),
            (Expected::Summary(expected), _) => {
                let actual = summary(&value);
                if &actual != expected {
                    failures.push(format!(
                        "line {}: `{}`\n    expected {expected}\n    actual   {actual}",
                        marker.line, marker.expression
                    ));
                }
                if presentation.shape == PresentedShape::Sequence {
                    check_children(&scenario, marker, presentation, &mut failures).await;
                }
            }
        }
    }
    assert!(failures.is_empty(), "{fixture}:\n{}", failures.join("\n"));
    scenario.shutdown().await;
    seen
}

/// Every built-in view of `library` presented some marked value.
fn assert_every_view_binds(library: &str, seen: &BTreeSet<String>) {
    let missing = uscope::built_in_views()
        .into_iter()
        .filter(|view| &*view.source == library)
        .map(|view| view.to_string())
        .filter(|view| !seen.contains(view))
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "{library}: no fixture value binds {missing:?}"
    );
}

#[tokio::test]
async fn cpp_containers_present_as_their_views_say_across_the_library_matrix() {
    let mut seen = BTreeSet::new();
    for (fixture, optimized) in [
        ("containers-cpp-gcc-o0", false),
        ("containers-cpp-gcc-o2", true),
        ("containers-cpp-clang-o0", false),
        ("containers-cpp-clang-o2", true),
        ("containers-cpp-gcc-oldabi", false),
        ("containers-cpp-libcxx-o0", false),
        ("containers-cpp-libcxx-o2", true),
    ] {
        seen.extend(check_containers(fixture, "cpp/containers.cpp", "barrier", optimized).await);
    }
    assert_every_view_binds("libstdc++.views", &seen);
    assert_every_view_binds("libc++.views", &seen);
}

#[tokio::test]
async fn rust_containers_present_as_their_views_say() {
    let mut seen = BTreeSet::new();
    for (fixture, optimized) in [("containers-rust-o0", false), ("containers-rust-o2", true)] {
        seen.extend(check_containers(fixture, "rust/containers.rs", "barrier", optimized).await);
    }
    assert_every_view_binds("rust-std.views", &seen);
}

#[tokio::test]
async fn zig_containers_present_as_their_views_say() {
    let mut seen = BTreeSet::new();
    for (fixture, optimized) in [("containers-zig-o0", false), ("containers-zig-o2", true)] {
        seen.extend(check_containers(fixture, "zig/containers.zig", "barrier", optimized).await);
    }
    assert_every_view_binds("zig-std.views", &seen);
}

/// Inspection sent beside run control never holds it up or answers
/// wrongly: whichever the controller serves first, run control is
/// acknowledged, and each inspection is either its whole answer at the stop
/// or a failure for an old stop, never part of one or a different one.
#[tokio::test]
async fn inspection_sent_beside_run_control_is_whole_or_stale() {
    for round in 0..4 {
        let mut scenario = Scenario::launch("containers-rust-o0");
        scenario.add_breakpoint("barrier").await;
        scenario.run_to_stop().await;
        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        let caller = trace.frames[1].id;
        let snapshot = scenario.snapshot().await;
        let InferiorState::Stopped {
            process_id,
            stop_id,
            thread_id,
            ..
        } = snapshot.inferior
        else {
            panic!("round {round}: not stopped: {:?}", snapshot.inferior);
        };
        let context = uscope::StopContext {
            stop: stop_id,
            thread: thread_id,
            frame: caller,
        };
        let handle = scenario.handle().clone();
        let words = Expression::parse("words").expect("an expression");
        let many = Expression::parse("many[299] + len(many)").expect("an expression");
        let view = handle.at(context);
        let (words, many, resumed) = tokio::join!(
            view.evaluate(&words),
            view.evaluate(&many),
            handle.continue_execution(
                stop_id,
                uscope::ResumeScope::Process(process_id),
                uscope::ExceptionDisposition::Pass,
            ),
        );
        resumed.unwrap_or_else(|error| panic!("round {round}: continue failed: {error}"));
        for (result, expected) in [(words, "len=2 [\"one\", \"two\"]"), (many, "599")] {
            match result {
                Ok(Evaluation::Value { value, .. }) => {
                    let shown = match &value.state {
                        VariableState::Available {
                            presentation: Some(presentation),
                            ..
                        } => presentation.summary.to_string(),
                        VariableState::Available {
                            value: uscope::VariableValue::Scalar(ScalarValue::Signed(number)),
                            ..
                        } => number.to_string(),
                        VariableState::Available {
                            value: uscope::VariableValue::Scalar(ScalarValue::Unsigned(number)),
                            ..
                        } => number.to_string(),
                        state => format!("{state:?}"),
                    };
                    assert_eq!(shown, expected, "round {round}");
                }
                Err(Error::StaleStop | Error::NotStopped | Error::NotRunning) => {}
                other => panic!("round {round}: {other:?}"),
            }
        }
        scenario.shutdown().await;
    }
}

/// Views loaded for a session come before the built-in ones; a view that
/// presents a value as another lends it that value's children; and what
/// keeps a view out of a file is reported with where it is.
#[tokio::test]
async fn session_views_come_first_and_present_values_as_others() {
    let mut scenario = Scenario::launch("containers-rust-o0");
    scenario.add_breakpoint("barrier").await;
    scenario.run_to_stop().await;
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    scenario
        .operation(
            "select caller",
            scenario.handle().select_frame(trace.frames[1].id),
        )
        .await;
    let errors = scenario
        .operation(
            "load views",
            scenario.handle().load_views(&[(
                "session.views",
                "uscope-views 1
view rust std::**::PathBuf {
    show value(inner(self))
    field length = inner(self).len
}
view rust alloc::vec::Vec<T, _> {
    show empty(\"a vector\")
}
view rust nowhere {
    show nothing
}
",
            )]),
        )
        .await;
    assert_eq!(
        errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
        [
            "session.views:10:10: expected a shape: `text`, `value`, `empty`, `sequence`, or `if`, found `nothing`"
        ]
    );
    let ints = evaluate(&scenario, "ints").await;
    assert_eq!(summary(&ints), "a vector");
    // The path presents as its bytes, a Vec<u8> the session's view now
    // presents, whose children it borrows.
    let path = evaluate(&scenario, "path").await;
    let presentation = presentation(&path).expect("presented");
    assert_eq!(
        (presentation.shape, presentation.summary.as_ref()),
        (PresentedShape::Value, "a vector")
    );
    let ValueChildren::Available(reference) = &presentation.children else {
        panic!("{presentation:?}");
    };
    let page = scenario
        .operation(
            "children",
            scenario.handle().value_children(
                Arc::clone(reference),
                ValueChildQuery {
                    offset: 0,
                    limit: 8,
                },
            ),
        )
        .await;
    let names = page
        .children
        .iter()
        .map(|child| match &child.relationship {
            ValueChildRelationship::Field { name } => name.to_string(),
            ValueChildRelationship::Raw => "[raw]".to_owned(),
            other => format!("{other:?}"),
        })
        .collect::<Vec<_>>();
    // The vector's own presentation, `empty`, has no elements: its children
    // are its `[raw]`, then the path's field and `[raw]`.
    assert_eq!(names, ["[raw]", "length", "[raw]"]);
    let length = &page.children[1];
    assert!(
        matches!(
            length.state,
            VariableState::Available {
                value: uscope::VariableValue::Scalar(ScalarValue::Unsigned(11)),
                ..
            }
        ),
        "{length:?}"
    );
    // Restoring the built-in views presents the vector again.
    scenario
        .operation("unload views", scenario.handle().load_views(&[]))
        .await;
    assert_eq!(
        summary(&evaluate(&scenario, "ints").await),
        "len=3 [1, 2, 3]"
    );
    scenario.shutdown().await;
}
