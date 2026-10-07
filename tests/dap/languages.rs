//! Values in every supported language as a client sees them.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, fixture, source};

/// Launches a program to its first stop, returning the stopped thread and
/// its innermost frame.
fn stop(dap: &mut Dap, program: &str, configuration: &Configuration) -> (i64, Value) {
    let started = dap.launch(Profile::VsCode, &fixture(program), json!({}), configuration);
    let stop = dap.stopped(started.mark);
    assert!(stop.reason.contains("breakpoint"), "{program}: {stop:?}");
    let trace = dap.request("stackTrace", json!({"threadId": stop.thread, "levels": 1}));
    (stop.thread, trace["stackFrames"][0].clone())
}

/// The variables of a frame's argument and local scopes, by name.
fn frame_variables(dap: &mut Dap, frame: &Value) -> BTreeMap<String, Value> {
    let scopes = dap.request("scopes", json!({"frameId": frame["id"]}));
    let mut variables = BTreeMap::new();
    for scope in scopes["scopes"].as_array().expect("scopes") {
        if matches!(scope["name"].as_str(), Some("Arguments" | "Locals")) {
            let listed = dap.request(
                "variables",
                json!({"variablesReference": scope["variablesReference"]}),
            );
            for variable in listed["variables"].as_array().expect("variables") {
                variables.insert(
                    variable["name"].as_str().expect("name").to_owned(),
                    variable.clone(),
                );
            }
        }
    }
    variables
}

fn finish_running(mut dap: Dap, thread: i64) {
    let resumed = dap.send("continue", json!({"threadId": thread}));
    dap.success(resumed);
    assert_eq!(
        dap.event(resumed.mark, "exited", |_| true),
        json!({"exitCode": 0})
    );
    dap.finish();
}

#[test]
fn scalars_read_alike_in_cpp_rust_zig_and_go() {
    let snake = [
        "flag",
        "signed_value",
        "unsigned_value",
        "single",
        "double_precision",
        "local_signed",
    ];
    let camel = [
        "flag",
        "signedValue",
        "unsignedValue",
        "single",
        "doublePrecision",
        "localSigned",
    ];
    for (program, path, line, names) in [
        ("variables-cpp-gcc-o0", "cpp/variables.cpp", 27, snake),
        ("variables-cpp-clang-o0", "cpp/variables.cpp", 27, snake),
        ("variables-rust-o0", "rust/variables.rs", 33, snake),
        ("variables-zig-o0", "zig/variables.zig", 37, snake),
        ("variables-go-o0", "go/variables/main.go", 49, camel),
    ] {
        let mut dap = Dap::start(program);
        let (thread, frame) = stop(
            &mut dap,
            program,
            &Configuration {
                sources: vec![(source(path), vec![line])],
                ..Configuration::default()
            },
        );
        let variables = frame_variables(&mut dap, &frame);
        let values = names
            .iter()
            .map(|name| {
                variables
                    .get(*name)
                    .unwrap_or_else(|| panic!("{program} shows no {name}: {variables:?}"))["value"]
                    .clone()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            ["true", "-42", "42", "1.25", "-2.5", "-41"].map(Value::from),
            "{program}"
        );
        finish_running(dap, thread);
    }
}

/// Expands every value under a reference, to a bounded depth, and checks
/// that each child's `evaluateName` evaluates to the value shown for it.
fn assert_children_evaluate(
    dap: &mut Dap,
    frame: &Value,
    reference: &Value,
    depth: usize,
    checked: &mut usize,
) {
    let children = dap.request(
        "variables",
        json!({"variablesReference": reference, "start": 0, "count": 8}),
    );
    for child in children["variables"].as_array().expect("variables") {
        if let Some(expression) = child["evaluateName"].as_str() {
            let evaluated = dap.request(
                "evaluate",
                json!({"expression": expression, "frameId": frame["id"], "context": "watch"}),
            );
            assert_eq!(
                evaluated["result"], child["value"],
                "{expression} evaluates to something other than its row shows"
            );
            *checked += 1;
        }
        if depth > 0
            && child["variablesReference"]
                .as_i64()
                .is_some_and(|id| id > 0)
        {
            assert_children_evaluate(dap, frame, &child["variablesReference"], depth - 1, checked);
        }
    }
}

#[test]
fn records_and_enums_expand_and_their_members_evaluate_to_what_they_show() {
    for (program, function) in [
        ("records-cpp-gcc-o0", "inspect_records"),
        ("records-rust-o0", "inspect_records"),
        ("records-zig-o0", "inspectRecords"),
        ("records-go-o0", "main.inspectRecords"),
        ("enums-cpp-gcc-o0", "inspect_enums"),
        ("enums-rust-o0", "inspect_enum"),
        ("enums-zig-o0", "inspectEnums"),
        ("enums-go-o0", "main.inspectEnums"),
    ] {
        let mut dap = Dap::start(program);
        let (thread, frame) = stop(
            &mut dap,
            program,
            &Configuration {
                functions: vec![function.to_owned()],
                ..Configuration::default()
            },
        );
        let variables = frame_variables(&mut dap, &frame);
        assert!(!variables.is_empty(), "{program} shows no variables");
        let mut checked = 0;
        for variable in variables.values() {
            if variable["variablesReference"]
                .as_i64()
                .is_some_and(|id| id > 0)
            {
                assert_children_evaluate(
                    &mut dap,
                    &frame,
                    &variable["variablesReference"],
                    2,
                    &mut checked,
                );
            }
        }
        assert!(checked > 0, "{program} has no members to evaluate");
        finish_running(dap, thread);
    }
}

#[test]
fn step_out_shows_what_the_function_returned() {
    for program in ["values-go-o0", "values-go-o2"] {
        let mut dap = Dap::start(program);
        let (thread, _) = stop(
            &mut dap,
            program,
            &Configuration {
                functions: vec!["main.returning".to_owned()],
                ..Configuration::default()
            },
        );
        let sent = dap.send("stepOut", json!({"threadId": thread}));
        dap.success(sent);
        let stopped = dap.stopped(sent.mark);
        assert_eq!(stopped.reason, "step", "{program}");
        let trace = dap.request("stackTrace", json!({"threadId": thread, "levels": 1}));
        let variables = frame_variables(&mut dap, &trace["stackFrames"][0]);
        // Returned values are listed with the frame's own, and no name
        // reaches them.
        let count = &variables["returned count"];
        assert_eq!(count["value"], "42", "{program}: {variables:?}");
        assert!(count.get("evaluateName").is_none(), "{program}: {count}");
        assert_eq!(variables["returned text"]["value"], "\"go\"", "{program}");
        assert_eq!(variables["returned failure"]["value"], "nil", "{program}");
        finish_running(dap, thread);
    }
}
