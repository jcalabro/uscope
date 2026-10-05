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
fn write_data_breakpoints_stop_when_the_value_changes_and_say_how() {
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
    // Re-sending the same data breakpoint keeps it and its id.
    let again = dap.request(
        "setDataBreakpoints",
        json!({"breakpoints": [{"dataId": info["dataId"], "accessType": "write"}]}),
    );
    assert_eq!(breakpoints(&again)[0]["id"], id);
    assert_eq!(
        next_change(&mut dap, stop.thread, &id),
        "watch_i32 changed from 1 to 2"
    );
    // Clients present `write` as "Break on Value Change": the store of 2
    // over 2 does not stop.
    assert_eq!(
        next_change(&mut dap, stop.thread, &id),
        "watch_i32 changed from 2 to 42"
    );
    // An access data breakpoint stops at the load in the next phase that
    // touches the value.
    let frame =
        dap.request("stackTrace", json!({"threadId": stop.thread}))["stackFrames"][0].clone();
    let info = dap.request(
        "dataBreakpointInfo",
        json!({"name": "watch_i32", "frameId": frame["id"]}),
    );
    let access = dap.request(
        "setDataBreakpoints",
        json!({"breakpoints": [{"dataId": info["dataId"], "accessType": "readWrite"}]}),
    );
    assert_eq!(breakpoints(&access)[0]["verified"], true);
    let access_id = breakpoints(&access)[0]["id"].clone();
    assert_ne!(access_id, id);
    assert_eq!(
        next_change(&mut dap, stop.thread, &access_id),
        "watch_i32 was accessed; it is 42"
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

#[test]
fn addresses_are_watched_as_memory_a_client_names_by_address() {
    let mut dap = Dap::start("address data breakpoints");
    let (stop, frame) = stopped_in(&mut dap, "watch-gcc-o0", "scalar_stores");
    dap.request("setFunctionBreakpoints", json!({"breakpoints": []}));
    let reference = dap.request(
        "evaluate",
        json!({"expression": "watch_i32", "frameId": frame["id"], "context": "watch"}),
    )["memoryReference"]
        .as_str()
        .expect("watch_i32 is in memory")
        .to_owned();
    let info = dap.request(
        "dataBreakpointInfo",
        json!({"name": reference, "asAddress": true, "bytes": 4}),
    );
    let address = u64::from_str_radix(reference.trim_start_matches("0x"), 16).expect("hex");
    assert_eq!(info["description"], format!("4 bytes at {address:#x}"));
    let set = dap.request(
        "setDataBreakpoints",
        json!({"breakpoints": [{"dataId": info["dataId"], "accessType": "write"}]}),
    );
    let id = breakpoints(&set)[0]["id"].clone();
    let change = next_change(&mut dap, stop.thread, &id);
    assert!(change.contains("changed"), "{change}");

    // A name that is no address cannot be watched as one.
    let refused = dap.request(
        "dataBreakpointInfo",
        json!({"name": "watch_i32", "asAddress": true}),
    );
    assert_eq!(refused["dataId"], Value::Null);
    assert_eq!(refused["description"], "'watch_i32' is not an address");
    dap.finish();
}

#[test]
fn the_store_mode_stops_at_every_store_even_of_the_value_held() {
    let mut dap = Dap::start("store mode");
    let (stop, frame) = stopped_in(&mut dap, "watch-gcc-o0", "scalar_stores");
    dap.request("setFunctionBreakpoints", json!({"breakpoints": []}));
    let info = dap.request(
        "dataBreakpointInfo",
        json!({"name": "watch_i32", "frameId": frame["id"]}),
    );
    let set = dap.request(
        "setDataBreakpoints",
        json!({"breakpoints": [
            {"dataId": info["dataId"], "accessType": "write", "mode": "store"},
            {"dataId": info["dataId"], "accessType": "readWrite", "mode": "change"},
            {"dataId": info["dataId"], "accessType": "write", "mode": "sometimes"},
        ]}),
    );
    let rows = breakpoints(&set);
    assert_eq!(rows[0]["verified"], true);
    for (row, message) in [
        (
            &rows[1],
            "the change mode applies only to write data breakpoints",
        ),
        (&rows[2], "unknown data breakpoint mode 'sometimes'"),
    ] {
        assert_eq!(
            (&row["verified"], &row["message"]),
            (&json!(false), &json!(message))
        );
    }
    let id = rows[0]["id"].clone();
    for description in [
        "watch_i32 changed from 0 to 1",
        "watch_i32 changed from 1 to 2",
        "watch_i32 was written; it is still 2",
        "watch_i32 changed from 2 to 42",
    ] {
        assert_eq!(next_change(&mut dap, stop.thread, &id), description);
    }
    dap.finish();
}

#[test]
fn data_breakpoints_the_console_removes_are_removed_from_the_client() {
    let mut dap = Dap::start("console unwatch");
    let (stop, frame) = stopped_in(&mut dap, "watch-gcc-o0", "scalar_stores");
    dap.request("setFunctionBreakpoints", json!({"breakpoints": []}));
    let info = dap.request(
        "dataBreakpointInfo",
        json!({"name": "watch_i32", "frameId": frame["id"]}),
    );
    let set = dap.request(
        "setDataBreakpoints",
        json!({"breakpoints": [{"dataId": info["dataId"], "accessType": "write"}]}),
    );
    let id = breakpoints(&set)[0]["id"].clone();
    let mark = dap.mark();
    let output = dap.request(
        "evaluate",
        json!({"expression": "unwatch all", "frameId": frame["id"], "context": "repl"}),
    );
    assert!(
        output["result"]
            .as_str()
            .is_some_and(|text| text.contains("1 watchpoint")),
        "{output}"
    );
    let removed = dap.event(mark, "breakpoint", |body| body["reason"] == "removed");
    assert_eq!(removed["breakpoint"]["id"], id);
    // The client's next update has nothing left to release.
    let cleared = dap.request("setDataBreakpoints", json!({"breakpoints": []}));
    assert_eq!(cleared, json!({"breakpoints": []}));
    // The program's later phases raise signals of their own.
    dap.request("setExceptionBreakpoints", json!({"filters": []}));
    let resumed = dap.send("continue", json!({"threadId": stop.thread}));
    dap.success(resumed);
    let (kind, _) = dap.next_event(resumed.mark, &["stopped", "exited"]);
    assert_eq!(kind, "exited");
    dap.finish();
}

#[test]
fn data_breakpoints_before_a_program_is_loaded_wait_unverified() {
    let mut dap = Dap::start("data breakpoints before launch");
    dap.initialize(Profile::DeferredLaunch);
    assert_eq!(
        dap.request("setDataBreakpoints", json!({"breakpoints": []})),
        json!({"breakpoints": []})
    );
    let set = dap.request(
        "setDataBreakpoints",
        json!({"breakpoints": [{"dataId": "data-1", "accessType": "write"}]}),
    );
    assert_eq!(
        (
            &breakpoints(&set)[0]["verified"],
            &breakpoints(&set)[0]["reason"]
        ),
        (&json!(false), &json!("failed"))
    );
    dap.finish();
}
