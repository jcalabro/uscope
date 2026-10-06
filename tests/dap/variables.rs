//! Scopes, variables, and expressions as clients show them.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, Stopped, fixture, line_of, source};

/// Launches a fixture stopped at a breakpoint on the line holding `marker`.
fn stopped_at(dap: &mut Dap, program: &str, path: &str, marker: &str) -> Stopped {
    let path = source(path);
    let started = dap.launch(
        Profile::VsCode,
        &fixture(program),
        json!({}),
        &Configuration {
            sources: vec![(path.clone(), vec![line_of(&path, marker)])],
            ..Configuration::default()
        },
    );
    dap.stopped(started.mark)
}

fn frames(dap: &mut Dap, thread: i64) -> Vec<Value> {
    dap.request("stackTrace", json!({"threadId": thread}))["stackFrames"]
        .as_array()
        .expect("frames")
        .clone()
}

/// Every scope of a frame by name.
fn scopes(dap: &mut Dap, frame: &Value) -> BTreeMap<String, Value> {
    dap.request("scopes", json!({"frameId": frame["id"]}))["scopes"]
        .as_array()
        .expect("scopes")
        .iter()
        .map(|scope| {
            (
                scope["name"].as_str().expect("name").to_owned(),
                scope.clone(),
            )
        })
        .collect()
}

fn variables(dap: &mut Dap, reference: &Value) -> Vec<Value> {
    dap.request("variables", json!({"variablesReference": reference}))["variables"]
        .as_array()
        .expect("variables")
        .clone()
}

fn named<'a>(variables: &'a [Value], name: &str) -> &'a Value {
    variables
        .iter()
        .find(|variable| variable["name"] == name)
        .unwrap_or_else(|| panic!("no variable {name} in {variables:?}"))
}

/// Variables as (name, value) pairs.
fn values(variables: &[Value]) -> Vec<(String, String)> {
    variables
        .iter()
        .map(|variable| {
            (
                variable["name"].as_str().expect("name").to_owned(),
                variable["value"].as_str().expect("value").to_owned(),
            )
        })
        .collect()
}

#[test]
fn variables_show_values_and_types_and_expand_aggregates() {
    let mut dap = Dap::start("variables");
    let stop = stopped_at(
        &mut dap,
        "variables-gcc-o0",
        "c/variables.c",
        "return **pointer_pointer",
    );
    let frame = frames(&mut dap, stop.thread)[0].clone();
    assert_eq!(frame["name"], "pointer_target");
    let scopes = scopes(&mut dap, &frame);
    assert_eq!(scopes["Arguments"]["presentationHint"], "arguments");
    assert_eq!(scopes["Locals"]["presentationHint"], "locals");
    assert_eq!(scopes["Registers"]["expensive"], true);

    let arguments = variables(&mut dap, &scopes["Arguments"]["variablesReference"]);
    assert_eq!(
        values(&arguments)[0],
        ("parameter".to_owned(), "40".to_owned())
    );
    let parameter = named(&arguments, "parameter");
    assert_eq!(
        (&parameter["type"], &parameter["evaluateName"]),
        (&json!("int"), &json!("parameter"))
    );
    assert_eq!(parameter["variablesReference"], 0);
    // A pointer to a scalar expands to what it points to.
    let pointer = named(&arguments, "pointer_parameter");
    assert_eq!(pointer["type"], "int *");
    assert_eq!(
        values(&variables(&mut dap, &pointer["variablesReference"])),
        [("*pointer_parameter".to_owned(), "42".to_owned())]
    );

    let locals = variables(&mut dap, &scopes["Locals"]["variablesReference"]);
    // Records and arrays say how many children they have.
    let pair = named(&locals, "pair");
    assert_eq!(
        (&pair["value"], &pair["namedVariables"]),
        (&json!("{<2 fields>}"), &json!(2))
    );
    let fields = variables(&mut dap, &pair["variablesReference"]);
    assert_eq!(
        values(&fields),
        [
            ("first".to_owned(), "20".to_owned()),
            ("second".to_owned(), "22".to_owned())
        ]
    );
    assert_eq!(fields[1]["evaluateName"], "pair.second");
    let array = named(&locals, "array");
    assert_eq!(
        (&array["value"], &array["indexedVariables"]),
        (&json!("[<2 elements>]"), &json!(2))
    );
    let elements = variables(&mut dap, &array["variablesReference"]);
    assert_eq!(
        values(&elements),
        [
            ("[0]".to_owned(), "20".to_owned()),
            ("[1]".to_owned(), "22".to_owned())
        ]
    );
    assert_eq!(elements[1]["evaluateName"], "array[1]");
    for child in fields.iter().chain(&elements) {
        assert_evaluates(&mut dap, &frame, child);
    }
    dap.finish();
}

#[test]
fn pointers_expand_to_what_they_point_to_or_say_why_not() {
    let mut dap = Dap::start("pointers");
    let stop = stopped_at(
        &mut dap,
        "variables-gcc-o0",
        "c/variables.c",
        "return **pointer_pointer",
    );
    let frame = frames(&mut dap, stop.thread)[0].clone();
    let reference = scopes(&mut dap, &frame)["Locals"]["variablesReference"].clone();
    let locals = variables(&mut dap, &reference);
    // A pointer to a record expands straight to the record's members.
    let structure = named(&locals, "structure_pointer");
    let members = variables(&mut dap, &structure["variablesReference"]);
    assert_eq!(
        values(&members),
        [
            ("first".to_owned(), "20".to_owned()),
            ("second".to_owned(), "22".to_owned())
        ]
    );
    assert_eq!(members[0]["evaluateName"], "(*structure_pointer).first");
    // A list can be walked node by node.
    let mut node = named(&locals, "recursive_pointer").clone();
    for expected in ["40", "41", "42"] {
        let fields = variables(&mut dap, &node["variablesReference"]);
        assert_eq!(named(&fields, "value")["value"], expected);
        node = named(&fields, "next").clone();
    }
    assert_eq!(
        node["variablesReference"], 0,
        "the last node's next is null"
    );
    // Values that cannot be followed say why, as values.
    let null = named(&locals, "null_pointer");
    assert_eq!(
        (&null["value"], &null["variablesReference"]),
        (&json!("0x0000000000000000"), &json!(0))
    );
    let invalid = named(&locals, "invalid_pointer");
    let target = variables(&mut dap, &invalid["variablesReference"]);
    assert_eq!(target[0]["name"], "*invalid_pointer");
    assert!(
        target[0]["value"]
            .as_str()
            .is_some_and(|value| value.starts_with("<unavailable")),
        "{target:?}"
    );
    // Every child's evaluate name evaluates to the same value.
    for child in &members {
        assert_evaluates(&mut dap, &frame, child);
    }
    dap.finish();
}

/// Checks that a variable's evaluate name evaluates to its value.
fn assert_evaluates(dap: &mut Dap, frame: &Value, variable: &Value) {
    let result = dap.request(
        "evaluate",
        json!({"expression": variable["evaluateName"], "frameId": frame["id"], "context": "watch"}),
    );
    assert_eq!(result["result"], variable["value"], "{variable}");
}

#[test]
fn caller_frames_registers_and_hexadecimal_values() {
    let mut dap = Dap::start("caller frames");
    let stop = stopped_at(
        &mut dap,
        "variables-gcc-o0",
        "c/variables.c",
        "return **pointer_pointer",
    );
    let frames = frames(&mut dap, stop.thread);
    assert_eq!(frames[1]["name"], "main");
    let caller = scopes(&mut dap, &frames[1]);
    assert!(!caller.contains_key("Arguments"), "main takes no arguments");
    let locals = variables(&mut dap, &caller["Locals"]["variablesReference"]);
    for (name, value, type_name) in [
        ("boolean", "true", "_Bool"),
        ("character", "65 'A'", "char"),
        ("signed_int", "-1234567", "int"),
        ("single", "1.25", "float"),
        ("extended", "3.125", "long double"),
    ] {
        let variable = named(&locals, name);
        assert_eq!(
            (&variable["value"], &variable["type"]),
            (&json!(value), &json!(type_name)),
            "{name}"
        );
    }
    // The innermost frame's program counter is its instruction pointer.
    let innermost = scopes(&mut dap, &frames[0]);
    let registers = variables(&mut dap, &innermost["Registers"]["variablesReference"]);
    let rip = named(&registers, "rip")["value"]
        .as_str()
        .expect("rip")
        .to_owned();
    let pointer = frames[0]["instructionPointerReference"]
        .as_str()
        .expect("pointer")
        .to_owned();
    let number = |text: &str| u64::from_str_radix(text.trim_start_matches("0x"), 16).expect("hex");
    assert_eq!(number(&rip), number(&pointer));
    // A caller's registers that its callees may change are unknown.
    let caller_registers = variables(&mut dap, &caller["Registers"]["variablesReference"]);
    assert_eq!(named(&caller_registers, "rax")["value"], "<not saved>");
    // Values can be shown in hexadecimal.
    let reference = innermost["Arguments"]["variablesReference"].clone();
    let arguments = dap.request(
        "variables",
        json!({"variablesReference": reference, "format": {"hex": true}}),
    );
    assert_eq!(arguments["variables"][0]["value"], "0x28");
    dap.finish();
}

#[test]
fn large_arrays_are_read_in_the_pages_clients_ask_for() {
    let mut dap = Dap::start("paging");
    let stop = stopped_at(
        &mut dap,
        "records-c-gcc-o0",
        "c/records.c",
        "volatile int marker",
    );
    let frames = frames(&mut dap, stop.thread);
    let caller = scopes(&mut dap, &frames[1]);
    let locals = variables(&mut dap, &caller["Locals"]["variablesReference"]);
    let large = variables(&mut dap, &named(&locals, "large")["variablesReference"]);
    let padding = named(&large, "padding");
    assert_eq!(padding["indexedVariables"], 2048);
    // VS Code asks for big arrays a hundred elements at a time.
    let page = dap.request(
        "variables",
        json!({"variablesReference": padding["variablesReference"], "filter": "indexed", "start": 1000, "count": 100}),
    )["variables"]
        .as_array()
        .expect("page")
        .clone();
    assert_eq!(page.len(), 100);
    assert_eq!(page[0]["name"], "[1000]");
    assert_eq!(page[99]["name"], "[1099]");
    assert_eq!(page[99]["evaluateName"], "large.padding[1099]");
    assert!(page.iter().all(|element| element["value"] == "0"));
    // Any window holds exactly the elements it covers; a count of zero, as
    // the protocol says, means every element from the start.
    for (start, count, expected) in [
        (0, 1, 1),
        (7, 13, 13),
        (1999, 1, 1),
        (2040, 100, 8),
        (2000, 0, 48),
    ] {
        let window = dap.request(
            "variables",
            json!({"variablesReference": padding["variablesReference"], "filter": "indexed", "start": start, "count": count}),
        )["variables"]
            .as_array()
            .expect("window")
            .clone();
        let names = window
            .iter()
            .map(|element| element["name"].as_str().expect("name").to_owned())
            .collect::<Vec<_>>();
        let wanted = (start..start + expected)
            .map(|index| format!("[{index}]"))
            .collect::<Vec<_>>();
        assert_eq!(names, wanted, "start {start}, count {count}");
    }
    // Past the end there is nothing.
    let past = dap.request(
        "variables",
        json!({"variablesReference": padding["variablesReference"], "start": 2048, "count": 10}),
    );
    assert_eq!(past["variables"], json!([]));
    dap.finish();
}

#[test]
fn optimized_out_values_explain_themselves_as_values() {
    let mut dap = Dap::start("optimized");
    let started = dap.launch(
        Profile::Neovim,
        &fixture("variables-gcc-o2"),
        json!({}),
        &Configuration {
            functions: vec!["pointer_target".to_owned()],
            ..Configuration::default()
        },
    );
    let stop = dap.stopped(started.mark);
    let frame = frames(&mut dap, stop.thread)[0].clone();
    let reference = scopes(&mut dap, &frame)["Locals"]["variablesReference"].clone();
    let locals = variables(&mut dap, &reference);
    let unavailable = named(&locals, "pointer_pointer");
    assert_eq!(
        unavailable["value"],
        "<unavailable: the value is unavailable at the current instruction>"
    );
    assert_eq!(unavailable["variablesReference"], 0);
    // Following an unavailable pointer gives an unavailable value too.
    let result = dap.request(
        "evaluate",
        json!({"expression": "*pointer_pointer", "frameId": frame["id"], "context": "watch"}),
    );
    assert_eq!(result["result"], unavailable["value"]);
    assert_eq!(result["variablesReference"], 0);
    dap.finish();
}

#[test]
fn expressions_evaluate_for_watches_hovers_ranges_and_without_a_frame() {
    let mut dap = Dap::start("evaluate");
    let stop = stopped_at(
        &mut dap,
        "variables-gcc-o0",
        "c/variables.c",
        "return **pointer_pointer",
    );
    let frame = frames(&mut dap, stop.thread)[0]["id"].clone();
    for context in ["watch", "hover", "clipboard", "variables"] {
        let result = dap.request(
            "evaluate",
            json!({"expression": "pair.second", "frameId": frame, "context": context}),
        );
        assert_eq!(
            (&result["result"], &result["type"]),
            (&json!("22"), &json!("int")),
            "{context}"
        );
    }
    // Expressions compute, in every context.
    for context in ["watch", "hover", "clipboard", "variables", "repl"] {
        let result = dap.request(
            "evaluate",
            json!({"expression": "pair.second * 2 + array[0]", "frameId": frame, "context": context}),
        );
        assert_eq!(
            (&result["result"], &result["type"]),
            (&json!("64"), &json!("integer")),
            "{context}"
        );
    }
    assert_eq!(
        dap.request(
            "evaluate",
            json!({"expression": "(char)(pair.second + 300)", "frameId": frame, "context": "hover"})
        )["type"],
        "char"
    );
    assert_eq!(
        dap.request_error(
            "evaluate",
            json!({"expression": "pair.second / 0", "frameId": frame, "context": "watch"})
        ),
        "division by zero"
    );
    // Without a frame, the stopped thread's innermost frame is used.
    assert_eq!(
        dap.request(
            "evaluate",
            json!({"expression": "*pointer", "context": "watch"})
        )["result"],
        "42"
    );
    let range = dap.request(
        "evaluate",
        json!({"expression": "array[0..2]", "frameId": frame, "context": "watch"}),
    );
    assert_eq!(
        (&range["result"], &range["indexedVariables"]),
        (&json!("[<2 elements>]"), &json!(2))
    );
    assert_eq!(
        values(&variables(&mut dap, &range["variablesReference"])),
        [
            ("[0]".to_owned(), "20".to_owned()),
            ("[1]".to_owned(), "22".to_owned())
        ]
    );
    assert_eq!(
        dap.request_error(
            "evaluate",
            json!({"expression": "pair.", "frameId": frame, "context": "watch"})
        ),
        "expected a member name, found the end of the expression"
    );
    let unknown = dap.request_error(
        "evaluate",
        json!({"expression": "nonexistent", "frameId": frame, "context": "hover"}),
    );
    assert!(unknown.contains("nonexistent"), "{unknown}");
    dap.finish();
}

#[test]
fn references_from_an_earlier_stop_are_refused_after_resuming() {
    let mut dap = Dap::start("stale references");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("variables-gcc-o0"),
        json!({}),
        &Configuration {
            functions: vec!["pointer_target".to_owned()],
            ..Configuration::default()
        },
    );
    let stop = dap.stopped(started.mark);
    let frame = frames(&mut dap, stop.thread)[0].clone();
    let locals = scopes(&mut dap, &frame)["Locals"]["variablesReference"].clone();
    let pair = named(&variables(&mut dap, &locals), "pair")["variablesReference"].clone();
    let resumed = dap.send("continue", json!({"threadId": stop.thread}));
    dap.success(resumed);
    // The function is called twice, so the same breakpoint stops again.
    let again = dap.stopped(resumed.mark);
    let new_frame = frames(&mut dap, again.thread)[0].clone();
    assert_ne!(new_frame["id"], frame["id"], "references are never reused");
    let stale = |kind: &str, id: &Value| {
        format!("{kind} reference {id} is stale: it belongs to an earlier stop, or never existed")
    };
    assert_eq!(
        dap.request_error("scopes", json!({"frameId": frame["id"]})),
        stale("frame", &frame["id"])
    );
    assert_eq!(
        dap.request_error("variables", json!({"variablesReference": locals})),
        stale("variables", &locals)
    );
    assert_eq!(
        dap.request_error("variables", json!({"variablesReference": pair})),
        stale("variables", &pair)
    );
    assert_eq!(
        dap.request_error(
            "evaluate",
            json!({"expression": "pair", "frameId": frame["id"]})
        ),
        stale("frame", &frame["id"])
    );
    // Once the program is gone, every inspection says so.
    let resumed = dap.send("continue", json!({"threadId": again.thread}));
    dap.success(resumed);
    dap.event(resumed.mark, "terminated", |_| true);
    assert_eq!(
        dap.request_error("variables", json!({"variablesReference": pair})),
        "the program is not running"
    );
    dap.finish();
}

#[test]
fn strings_show_their_text_and_still_expand() {
    let mut dap = Dap::start("strings");
    let stop = stopped_at(
        &mut dap,
        "strings-c-gcc-o0",
        "c/strings.c",
        "strings stop here",
    );
    let frame = frames(&mut dap, stop.thread)[0].clone();
    let scopes = scopes(&mut dap, &frame);
    let arguments = variables(&mut dap, &scopes["Arguments"]["variablesReference"]);
    let greeting = named(&arguments, "greeting");
    let value = greeting["value"].as_str().expect("value");
    assert!(
        value.starts_with("0x") && value.ends_with(r#" "hello, world""#),
        "{value}"
    );
    // The pointer still leads to its first character.
    let pointee = variables(&mut dap, &greeting["variablesReference"]);
    assert_eq!(
        values(&pointee),
        [("*greeting".to_owned(), "104".to_owned())]
    );
    let locals = variables(&mut dap, &scopes["Locals"]["variablesReference"]);
    let buffer = named(&locals, "buffer");
    assert_eq!(
        (&buffer["value"], &buffer["indexedVariables"]),
        (&json!(r#""abc""#), &json!(16))
    );
    let evaluated = dap.request(
        "evaluate",
        json!({"expression": "edge", "frameId": frame["id"], "context": "hover"}),
    );
    assert!(
        evaluated["result"]
            .as_str()
            .is_some_and(|result| result.contains(r#""eeeee"... <unreadable at 0x"#))
    );
    dap.finish();
}

/// A value a view presents shows its summary, counts its elements as
/// indexed and its fields and `[raw]` as named, pages its elements by the
/// client's filter, and its elements evaluate back and can be changed.
#[test]
fn views_present_containers_with_paged_elements_and_raw_one_step_away() {
    let mut dap = Dap::start("views");
    let stop = stopped_at(
        &mut dap,
        "containers-rust-o0",
        "rust/containers.rs",
        "barrier(std::ptr",
    );
    let frame = frames(&mut dap, stop.thread)[0].clone();
    let scopes = scopes(&mut dap, &frame);
    let locals = variables(&mut dap, &scopes["Locals"]["variablesReference"]);

    let text = named(&locals, "text");
    assert_eq!(text["value"], r#""hello, world""#);
    assert!(
        text["presentationHint"]["attributes"]
            .as_array()
            .is_some_and(|attributes| attributes.contains(&json!("rawString"))),
        "{text}"
    );

    let many = named(&locals, "many");
    assert_eq!(
        many["value"],
        "len=300 [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, …]"
    );
    assert_eq!(
        (&many["indexedVariables"], &many["namedVariables"]),
        (&json!(300), &json!(2))
    );
    let page = |dap: &mut Dap, filter: &str, start: u64, count: u64| {
        dap.request(
            "variables",
            json!({
                "variablesReference": many["variablesReference"],
                "filter": filter,
                "start": start,
                "count": count,
            }),
        )["variables"]
            .as_array()
            .expect("variables")
            .clone()
    };
    let indexed = page(&mut dap, "indexed", 100, 50);
    assert_eq!(indexed.len(), 50);
    for (offset, row) in indexed.iter().enumerate() {
        let index = 100 + offset;
        assert_eq!(
            (&row["name"], &row["value"], &row["evaluateName"]),
            (
                &json!(format!("[{index}]")),
                &json!(index.to_string()),
                &json!(format!("many[{index}]"))
            ),
        );
    }
    let named_rows = page(&mut dap, "named", 0, 10);
    assert_eq!(
        values(&named_rows),
        [
            ("capacity".to_owned(), "300".to_owned()),
            ("[raw]".to_owned(), "{<2 fields>}".to_owned()),
        ]
    );
    let raw = variables(&mut dap, &named_rows[1]["variablesReference"]);
    assert_eq!(
        raw.iter()
            .map(|row| row["name"].clone())
            .collect::<Vec<_>>(),
        [json!("buf"), json!("len")]
    );

    // An element in memory is changed through its view.
    let ints = named(&locals, "ints");
    let elements = variables(&mut dap, &ints["variablesReference"]);
    assert_eq!(
        values(&elements[..3]),
        [
            ("[0]".to_owned(), "1".to_owned()),
            ("[1]".to_owned(), "2".to_owned()),
            ("[2]".to_owned(), "3".to_owned()),
        ]
    );
    let changed = dap.request(
        "setVariable",
        json!({"variablesReference": ints["variablesReference"], "name": "[1]", "value": "20"}),
    );
    assert_eq!(changed["value"], "20");
    let evaluated = dap.request(
        "evaluate",
        json!({"expression": "ints", "frameId": frame["id"], "context": "watch"}),
    );
    assert_eq!(
        (&evaluated["result"], &evaluated["indexedVariables"]),
        (&json!("len=3 [1, 20, 3]"), &json!(3))
    );
    dap.finish();
}

/// A map's entries are indexed and named by their keys; each entry's value
/// evaluates back by where it is, and can be changed. A scanned sequence's
/// elements evaluate back by their index.
#[test]
fn views_present_maps_as_entries_named_by_their_keys() {
    let mut dap = Dap::start("maps");
    let stop = stopped_at(
        &mut dap,
        "containers-cpp-gcc-o0",
        "cpp/containers.cpp",
        "barrier(&text)",
    );
    let frame = frames(&mut dap, stop.thread)[0].clone();
    let scopes = scopes(&mut dap, &frame);
    let locals = variables(&mut dap, &scopes["Locals"]["variablesReference"]);

    let ordered = named(&locals, "ordered");
    assert_eq!(ordered["value"], "len=3 {1: 10, 2: 20, 3: 30}");
    assert_eq!(
        (&ordered["indexedVariables"], &ordered["namedVariables"]),
        (&json!(3), &json!(1))
    );
    let entries = dap.request(
        "variables",
        json!({"variablesReference": ordered["variablesReference"], "filter": "indexed", "start": 0, "count": 3}),
    )["variables"]
        .as_array()
        .expect("variables")
        .clone();
    assert_eq!(
        values(&entries),
        [
            ("1".to_owned(), "10".to_owned()),
            ("2".to_owned(), "20".to_owned()),
            ("3".to_owned(), "30".to_owned()),
        ]
    );
    let place = entries[1]["evaluateName"]
        .as_str()
        .expect("an evaluateName");
    assert!(place.starts_with("*(int*)"), "{place}");
    let again = dap.request(
        "evaluate",
        json!({"expression": place, "frameId": frame["id"], "context": "watch"}),
    );
    assert_eq!(again["result"], "20");
    let changed = dap.request(
        "setVariable",
        json!({"variablesReference": ordered["variablesReference"], "name": "2", "value": "25"}),
    );
    assert_eq!(changed["value"], "25");
    let evaluated = dap.request(
        "evaluate",
        json!({"expression": "ordered", "frameId": frame["id"], "context": "watch"}),
    );
    assert_eq!(evaluated["result"], "len=3 {1: 10, 2: 25, 3: 30}");

    let linked = named(&locals, "linked");
    let elements = variables(&mut dap, &linked["variablesReference"]);
    assert_eq!(
        (&elements[2]["name"], &elements[2]["evaluateName"]),
        (&json!("[2]"), &json!("linked[2]"))
    );
    let third = dap.request(
        "evaluate",
        json!({"expression": "linked[2] + len(linked)", "frameId": frame["id"], "context": "watch"}),
    );
    assert_eq!(third["result"], "6");
    dap.finish();
}
