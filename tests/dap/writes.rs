//! Changing variables, expressions, and memory from the client.

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, fixture, line_of, source};

/// Stops the variables program on the line with `marker`, with no
/// breakpoints left, and returns its frames.
fn stopped(dap: &mut Dap, marker: &str) -> (i64, Vec<Value>) {
    let path = source("c/variables.c");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("variables-gcc-o0"),
        json!({}),
        &Configuration {
            sources: vec![(path.clone(), vec![line_of(&path, marker)])],
            ..Configuration::default()
        },
    );
    let stop = dap.stopped(started.mark);
    dap.request(
        "setBreakpoints",
        json!({"source": {"path": path}, "breakpoints": []}),
    );
    let trace = dap.request("stackTrace", json!({"threadId": stop.thread}));
    (
        stop.thread,
        trace["stackFrames"].as_array().expect("frames").clone(),
    )
}

fn scope(dap: &mut Dap, frame: &Value, name: &str) -> Value {
    dap.request("scopes", json!({"frameId": frame["id"]}))["scopes"]
        .as_array()
        .expect("scopes")
        .iter()
        .find(|scope| scope["name"] == name)
        .expect("scope")["variablesReference"]
        .clone()
}

fn row(dap: &mut Dap, reference: &Value, name: &str) -> Value {
    dap.request("variables", json!({"variablesReference": reference}))["variables"]
        .as_array()
        .expect("variables")
        .iter()
        .find(|variable| variable["name"] == name)
        .unwrap_or_else(|| panic!("no {name}"))
        .clone()
}

#[test]
fn set_variable_changes_values_the_program_then_uses() {
    let mut dap = Dap::start("set variable");
    let (thread, frames) = stopped(&mut dap, "return **pointer_pointer");
    let locals = scope(&mut dap, &frames[0], "Locals");
    let pointee = row(&mut dap, &locals, "pointee");
    // Values with storage are editable; aggregates are not.
    assert_eq!(pointee["presentationHint"]["attributes"], json!([]));
    let pair = row(&mut dap, &locals, "pair");
    assert_eq!(pair["presentationHint"]["attributes"], json!(["readOnly"]));

    let mark = dap.mark();
    let set = dap.request(
        "setVariable",
        json!({"variablesReference": locals, "name": "pointee", "value": "pointee + 58"}),
    );
    assert_eq!(
        (&set["value"], &set["type"]),
        (&json!("100"), &json!("int"))
    );
    // The client is told to read values again.
    dap.event(mark, "invalidated", |body| {
        body["areas"] == json!(["variables"])
    });
    let through = dap.request(
        "evaluate",
        json!({"expression": "*pointer", "frameId": frames[0]["id"], "context": "watch"}),
    );
    assert_eq!(through["result"], "100");
    // Expressions can be set too, here in the caller's frame.
    let caller = dap.request(
        "setExpression",
        json!({"expression": "signed_int", "value": "5", "frameId": frames[1]["id"]}),
    );
    assert_eq!(caller["value"], "5");
    let main_locals = scope(&mut dap, &frames[1], "Locals");
    let narrow = dap.request_error(
        "setVariable",
        json!({"variablesReference": main_locals, "name": "unsigned_character", "value": "300"}),
    );
    assert_eq!(
        narrow,
        "cannot assign to unsigned_character: 300 does not fit a unsigned 8-bit value"
    );
    let record = dap.request_error(
        "setVariable",
        json!({"variablesReference": locals, "name": "pair", "value": "1"}),
    );
    assert_eq!(
        record,
        "cannot assign to pair: only numbers, booleans, enumerations, and pointers can be assigned"
    );
    // main checks the values it set, and now finds them changed.
    let resumed = dap.send("continue", json!({"threadId": thread}));
    dap.success(resumed);
    assert_eq!(
        dap.event(resumed.mark, "exited", |_| true),
        json!({"exitCode": 1})
    );
    dap.finish();
}

#[test]
fn write_memory_writes_bytes_and_reports_the_change() {
    let mut dap = Dap::start("write memory");
    let (_, frames) = stopped(&mut dap, "return **pointer_pointer");
    let locals = scope(&mut dap, &frames[0], "Locals");
    let pointee = row(&mut dap, &locals, "pointee");
    let reference = pointee["memoryReference"].clone();
    let mark = dap.mark();
    // 7, little-endian.
    let written = dap.request(
        "writeMemory",
        json!({"memoryReference": reference, "data": "BwAAAA=="}),
    );
    assert_eq!(written["bytesWritten"], 4);
    let event = dap.event(mark, "memory", |_| true);
    assert_eq!(
        (&event["memoryReference"], &event["count"]),
        (&reference, &json!(4))
    );
    assert_eq!(row(&mut dap, &locals, "pointee")["value"], "7");
    let read = dap.request(
        "readMemory",
        json!({"memoryReference": reference, "count": 4}),
    );
    assert_eq!(read["data"], "BwAAAA==");
    assert_eq!(
        dap.request_error(
            "writeMemory",
            json!({"memoryReference": reference, "data": "not base64!"})
        ),
        "the data is not base64"
    );
    assert_eq!(
        dap.request_error(
            "writeMemory",
            json!({"memoryReference": "0x8", "data": "AA=="})
        ),
        "memory at 0x8 cannot be written"
    );
    dap.finish();
}
