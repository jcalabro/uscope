//! Source, function, and hit-count breakpoints as clients set them.

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, breakpoints, fixture, line_of, source};

fn set_lines(dap: &mut Dap, path: &std::path::Path, lines: &[u64]) -> Vec<Value> {
    breakpoints(&dap.request(
        "setBreakpoints",
        json!({
            "source": {"path": path},
            "breakpoints": lines.iter().map(|line| json!({"line": line})).collect::<Vec<_>>(),
        }),
    ))
}

#[test]
fn source_breakpoints_move_to_code_keep_their_ids_and_explain_failures() {
    let path = source("c/line-sliding.c");
    let comment = line_of(&path, "// a comment inside the function");
    let code = line_of(&path, "sliding_sink = doubled;");
    let between = line_of(&path, "// a comment between the functions");
    let mut dap = Dap::start("source breakpoints");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("line-sliding"),
        json!({"stopOnEntry": true}),
        &Configuration::default(),
    );
    let entry = dap.stopped(started.mark);

    let first = set_lines(&mut dap, &path, &[comment, between]);
    assert_eq!(first[0]["verified"], true);
    assert_eq!(
        first[0]["line"], code,
        "a line without code moves to the next"
    );
    assert_eq!(first[1]["verified"], false);
    assert_eq!(first[1]["reason"], "failed");
    assert_eq!(first[1]["line"], between);
    assert!(
        first[1]["message"]
            .as_str()
            .is_some_and(|message| message.contains("no code")),
        "{first:?}"
    );
    // Sending the same breakpoints again keeps their ids.
    let again = set_lines(&mut dap, &path, &[comment, between]);
    assert_eq!(again, first);
    // Sending the moved line, as dape does until lines settle, is stable.
    let settled = set_lines(&mut dap, &path, &[code]);
    assert_eq!(settled[0]["line"], code);
    assert_eq!(set_lines(&mut dap, &path, &[code]), settled);

    let resumed = dap.send("continue", json!({"threadId": entry.thread}));
    dap.success(resumed);
    let stop = dap.stopped(resumed.mark);
    assert_eq!(stop.body["hitBreakpointIds"], json!([settled[0]["id"]]));
    let frames = dap.inspect_as(Profile::VsCode, &stop);
    assert_eq!(
        (frames[0]["name"].clone(), frames[0]["line"].clone()),
        (json!("first_function"), json!(code))
    );
    // Clearing the file's breakpoints lets the program finish.
    assert!(set_lines(&mut dap, &path, &[]).is_empty());
    let resumed = dap.send("continue", json!({"threadId": stop.thread}));
    dap.success(resumed);
    assert_eq!(
        dap.event(resumed.mark, "exited", |_| true),
        json!({"exitCode": 0})
    );
    dap.finish();
}

#[test]
fn function_breakpoints_stop_at_every_function_with_the_name() {
    let mut dap = Dap::start("function breakpoints");
    let started = dap.launch(
        Profile::Neovim,
        &fixture("same-names"),
        json!({}),
        &Configuration {
            functions: vec![
                "helper".to_owned(),
                "no_such_function".to_owned(),
                "other.c:".to_owned(),
            ],
            ..Configuration::default()
        },
    );
    let [helper, missing, malformed] = &started.function_breakpoints[..] else {
        panic!("three breakpoints");
    };
    assert_eq!(helper["verified"], true);
    assert_eq!(missing["verified"], false);
    assert!(
        missing["message"]
            .as_str()
            .is_some_and(|message| message.contains("no_such_function"))
    );
    assert_eq!(
        malformed["message"],
        "'other.c:' is not a function, 0xaddress, file:line, or file:function"
    );
    let mut files = Vec::new();
    let mut mark = started.mark;
    for _ in 0..2 {
        let stop = dap.stopped(mark);
        assert_eq!(stop.reason, "function breakpoint");
        assert_eq!(stop.body["hitBreakpointIds"], json!([helper["id"]]));
        let frames = dap.inspect_as(Profile::Neovim, &stop);
        assert_eq!(frames[0]["name"], "helper");
        files.push(frames[0]["source"]["name"].clone());
        let resumed = dap.send("continue", json!({"threadId": stop.thread}));
        dap.success(resumed);
        mark = resumed.mark;
    }
    files.sort_by_key(ToString::to_string);
    assert_eq!(files, [json!("main.c"), json!("other.c")]);
    assert_eq!(dap.event(mark, "exited", |_| true), json!({"exitCode": 0}));
    dap.finish();
}

#[test]
fn hit_conditions_choose_which_hits_stop_and_reject_ambiguous_counts() {
    let mut dap = Dap::start("hit conditions");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("hit-counts-gcc-o0"),
        json!({"stopOnEntry": true}),
        &Configuration::default(),
    );
    let entry = dap.stopped(started.mark);
    let set = dap.request(
        "setFunctionBreakpoints",
        json!({"breakpoints": [
            {"name": "counted", "hitCondition": ">=38"},
            {"name": "caller", "hitCondition": "5"},
            {"name": "caller", "hitCondition": "%0"},
        ]}),
    );
    let [counted, bare, zero] = &breakpoints(&set)[..] else {
        panic!("three breakpoints");
    };
    assert_eq!(counted["verified"], true);
    assert_eq!(
        bare["message"],
        "invalid hit condition: a bare count is ambiguous; write ==5 to stop only at that hit or >=5 to stop at it and every later hit"
    );
    assert_eq!(
        zero["message"],
        "invalid hit condition: no hit can satisfy %0"
    );

    let mut mark = dap.send("continue", json!({"threadId": entry.thread})).mark;
    for call in 38..=40 {
        let stop = dap.stopped(mark);
        assert_eq!(stop.body["hitBreakpointIds"], json!([counted["id"]]));
        let trace = dap.request("stackTrace", json!({"threadId": stop.thread}));
        let frame = trace["stackFrames"][0]["id"].clone();
        let value = dap.request(
            "evaluate",
            json!({"expression": "call", "frameId": frame, "context": "watch"}),
        );
        assert_eq!(value["result"], call.to_string());
        let resumed = dap.send("continue", json!({"threadId": stop.thread}));
        dap.success(resumed);
        mark = resumed.mark;
    }
    assert_eq!(dap.event(mark, "exited", |_| true), json!({"exitCode": 0}));
    dap.finish();
}

#[test]
fn breakpoints_sharing_an_address_are_reported_by_one_stop() {
    let path = source("c/basic.c");
    let line = line_of(&path, "return uscope_value;");
    let mut dap = Dap::start("shared address");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("basic"),
        json!({}),
        &Configuration {
            sources: vec![(path, vec![line])],
            functions: vec!["breakpoint_target".to_owned()],
            ..Configuration::default()
        },
    );
    let source_id = started.source_breakpoints[0][0]["id"].clone();
    let function_id = started.function_breakpoints[0]["id"].clone();
    let stop = dap.stopped(started.mark);
    // Two kinds of breakpoint hit together make a plain breakpoint stop.
    assert_eq!(stop.reason, "breakpoint");
    let mut ids = stop.body["hitBreakpointIds"]
        .as_array()
        .expect("ids")
        .clone();
    ids.sort_by_key(Value::as_i64);
    assert_eq!(ids, [source_id.clone(), function_id]);
    // Removing one leaves the other installed.
    dap.request("setFunctionBreakpoints", json!({"breakpoints": []}));
    let resumed = dap.send("continue", json!({"threadId": stop.thread}));
    dap.success(resumed);
    let again = dap.stopped(resumed.mark);
    assert_eq!(again.body["hitBreakpointIds"], json!([source_id]));
    dap.finish();
}

#[test]
fn breakpoints_set_while_running_stop_it_and_cleared_ones_never_do() {
    let mut dap = Dap::start("edits while running");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("hot-calls"),
        json!({}),
        &Configuration::default(),
    );
    dap.event(started.mark, "process", |_| true);
    for _ in 0..10 {
        // Four threads call the function as fast as they can.
        let set = dap.send(
            "setFunctionBreakpoints",
            json!({"breakpoints": [{"name": "hot_function"}]}),
        );
        let id = breakpoints(&dap.success(set))[0]["id"].clone();
        let stop = dap.stopped(set.mark);
        assert_eq!(stop.body["hitBreakpointIds"], json!([id]));
        dap.inspect_as(Profile::VsCode, &stop);
        dap.request("setFunctionBreakpoints", json!({"breakpoints": []}));
        let resumed = dap.send("continue", json!({"threadId": stop.thread}));
        dap.success(resumed);
        // With the breakpoint gone, the next stop is the pause.
        let paused = dap.send("pause", json!({"threadId": stop.thread}));
        dap.success(paused);
        assert_eq!(dap.stopped(resumed.mark).reason, "pause");
        let resumed = dap.send("continue", json!({"threadId": stop.thread}));
        dap.success(resumed);
    }
    dap.finish();
}

#[test]
fn breakpoints_in_files_outside_the_program_fail_with_a_reason() {
    let mut dap = Dap::start("unrelated files");
    let started = dap.launch(
        Profile::Helix,
        &fixture("basic"),
        json!({"stopOnEntry": true}),
        &Configuration {
            sources: vec![
                ("/nonexistent/unrelated.c".into(), vec![3]),
                (source("c/spin.c"), vec![10]),
            ],
            ..Configuration::default()
        },
    );
    for breakpoints in &started.source_breakpoints {
        assert_eq!(breakpoints[0]["verified"], false);
        assert_eq!(breakpoints[0]["reason"], "failed");
        assert!(
            breakpoints[0]["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty())
        );
    }
    dap.stopped(started.mark);
    dap.finish();
}

#[test]
fn conditions_and_logpoints_are_refused_until_supported() {
    let path = source("c/basic.c");
    let mut dap = Dap::start("conditions");
    dap.initialize(Profile::VsCode);
    let launch = dap.send("launch", json!({"program": fixture("basic")}));
    let set = dap.request(
        "setBreakpoints",
        json!({"source": {"path": path}, "breakpoints": [
            {"line": 6, "condition": "first == 1"},
            {"line": 6, "logMessage": "hit {first}"},
        ]}),
    );
    let set = breakpoints(&set);
    assert_eq!(
        set[0]["message"],
        "conditional breakpoints are not supported yet"
    );
    assert_eq!(set[1]["message"], "logpoints are not supported yet");
    dap.request("configurationDone", Value::Null);
    dap.success(launch);
    assert_eq!(
        dap.event(launch.mark, "exited", |_| true),
        json!({"exitCode": 0})
    );
    dap.finish();
}
