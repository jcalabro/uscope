//! Data breakpoints: stopping when watched values change.

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, Stopped, breakpoints, fixture};

fn stopped_in(dap: &mut Dap, program: &str, function: &str) -> (Stopped, Value) {
    let started = dap.launch(
        Profile::VsCode,
        &fixture(program),
        json!({}),
        &Configuration {
            functions: vec![function.to_owned()],
            ..Configuration::default()
        },
    );
    let stop = dap.stopped(started.mark);
    let frame =
        dap.request("stackTrace", json!({"threadId": stop.thread}))["stackFrames"][0].clone();
    (stop, frame)
}

/// Continues and returns the data breakpoint stop's description.
fn next_change(dap: &mut Dap, thread: i64, id: &Value) -> String {
    let resumed = dap.send("continue", json!({"threadId": thread}));
    dap.success(resumed);
    let stop = dap.stopped(resumed.mark);
    assert_eq!(stop.reason, "data breakpoint");
    assert_eq!(stop.body["hitBreakpointIds"], json!([id]));
    stop.body["description"]
        .as_str()
        .expect("description")
        .to_owned()
}

#[test]
fn data_breakpoints_stop_at_every_store_and_say_what_changed() {
    let mut dap = Dap::start("data breakpoints");
    let (stop, frame) = stopped_in(&mut dap, "watch-gcc-o0", "scalar_stores");
    dap.request("setFunctionBreakpoints", json!({"breakpoints": []}));
    let info = dap.request(
        "dataBreakpointInfo",
        json!({"name": "watch_i32", "frameId": frame["id"]}),
    );
    assert_eq!(info["accessTypes"], json!(["write", "readWrite"]));
    assert_eq!(info["canPersist"], false);
    assert!(
        info["description"]
            .as_str()
            .is_some_and(|text| text.starts_with("watch_i32 (4 bytes at 0x"))
    );
    let set = dap.request(
        "setDataBreakpoints",
        json!({"breakpoints": [{"dataId": info["dataId"], "accessType": "write"}]}),
    );
    let id = breakpoints(&set)[0]["id"].clone();
    assert_eq!(breakpoints(&set)[0]["verified"], true);
    assert_eq!(
        next_change(&mut dap, stop.thread, &id),
        "watch_i32 changed from 0 to 1"
    );
    assert_eq!(
        next_change(&mut dap, stop.thread, &id),
        "watch_i32 changed from 1 to 2"
    );
    assert_eq!(
        next_change(&mut dap, stop.thread, &id),
        "watch_i32 was accessed; it is 2"
    );
    // Re-sending the same data breakpoint keeps it and its id.
    let again = dap.request(
        "setDataBreakpoints",
        json!({"breakpoints": [{"dataId": info["dataId"], "accessType": "write"}]}),
    );
    assert_eq!(breakpoints(&again)[0]["id"], id);
    assert_eq!(
        next_change(&mut dap, stop.thread, &id),
        "watch_i32 changed from 2 to 42"
    );
    // Cleared, it no longer stops: the next stop is the next phase's
    // breakpoint, although later phases store to the value again.
    dap.request("setDataBreakpoints", json!({"breakpoints": []}));
    dap.request("setExceptionBreakpoints", json!({"filters": []}));
    dap.request(
        "setFunctionBreakpoints",
        json!({"breakpoints": [{"name": "store_then_breakpoint"}]}),
    );
    let resumed = dap.send("continue", json!({"threadId": stop.thread}));
    dap.success(resumed);
    assert_eq!(dap.stopped(resumed.mark).reason, "function breakpoint");
    dap.finish();
}

#[test]
fn watching_a_local_from_the_variables_view_ends_with_its_frame() {
    let mut dap = Dap::start("local data breakpoint");
    let (stop, frame) = stopped_in(&mut dap, "watch-locals-gcc-o0", "leaf_local");
    dap.request("setFunctionBreakpoints", json!({"breakpoints": []}));
    let scopes = dap.request("scopes", json!({"frameId": frame["id"]}));
    let locals = scopes["scopes"]
        .as_array()
        .expect("scopes")
        .iter()
        .find(|scope| scope["name"] == "Locals")
        .expect("locals")["variablesReference"]
        .clone();
    dap.request("variables", json!({"variablesReference": locals}));
    // VS Code asks about a variable by its list and name.
    let info = dap.request(
        "dataBreakpointInfo",
        json!({"variablesReference": locals, "name": "local"}),
    );
    assert!(
        info["description"]
            .as_str()
            .is_some_and(|text| text.contains("until its function returns")),
        "{info}"
    );
    let set = dap.request(
        "setDataBreakpoints",
        json!({"breakpoints": [{"dataId": info["dataId"]}]}),
    );
    let id = breakpoints(&set)[0]["id"].clone();
    assert!(next_change(&mut dap, stop.thread, &id).ends_with("to 10"));
    assert!(next_change(&mut dap, stop.thread, &id).ends_with("from 10 to 11"));
    assert!(next_change(&mut dap, stop.thread, &id).ends_with("from 11 to 13"));
    // Once the function returns, the watch ends and the client hears so.
    let resumed = dap.send("continue", json!({"threadId": stop.thread}));
    dap.success(resumed);
    let removed = dap.event(resumed.mark, "breakpoint", |body| {
        body["reason"] == "removed"
    });
    assert_eq!(removed["breakpoint"]["id"], id);
    assert!(
        dap.output_containing(resumed.mark, "console", "was removed")
            .contains("its frame or block is no longer active")
    );
    // The stack slot's next use stops once to say the watch ended.
    let ended = dap.stopped(resumed.mark);
    assert_eq!(ended.reason, "data breakpoint");
    assert_eq!(
        ended.body["description"],
        "the watch on local ended: its frame or block is no longer active"
    );
    dap.finish();
}

#[test]
fn values_that_cannot_be_watched_say_why_without_failing() {
    let mut dap = Dap::start("unwatchable");
    let (_, frame) = stopped_in(&mut dap, "watch-gcc-o0", "scalar_stores");
    let missing = dap.request(
        "dataBreakpointInfo",
        json!({"name": "no_such_value", "frameId": frame["id"]}),
    );
    assert_eq!(missing["dataId"], Value::Null);
    assert!(
        missing["description"]
            .as_str()
            .is_some_and(|text| text.contains("no_such_value"))
    );
    let unknown_row = dap.request(
        "dataBreakpointInfo",
        json!({"name": "x", "variablesReference": 999_999}),
    );
    assert_eq!(unknown_row["dataId"], Value::Null);
    // An address and size can be watched directly.
    let info = dap.request(
        "dataBreakpointInfo",
        json!({"name": "0x1000", "asAddress": true, "bytes": 4}),
    );
    assert_eq!(info["description"], "4 bytes at 0x1000");
    // Watching reads alone is something x86 cannot do.
    let set = dap.request(
        "setDataBreakpoints",
        json!({"breakpoints": [
            {"dataId": info["dataId"], "accessType": "read"},
            {"dataId": "made-up"},
            {"dataId": info["dataId"], "condition": "x > 1"},
        ]}),
    );
    let set = breakpoints(&set);
    assert!(set.iter().all(|breakpoint| breakpoint["verified"] == false));
    assert!(
        set[0]["message"]
            .as_str()
            .is_some_and(|text| text.contains("read")),
        "{set:?}"
    );
    assert_eq!(
        set[1]["message"],
        "unknown data id 'made-up'; ask for it with dataBreakpointInfo"
    );
    assert_eq!(
        set[2]["message"],
        "conditions on data breakpoints are not supported"
    );
    dap.finish();
}
