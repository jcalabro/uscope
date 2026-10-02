//! The debug console runs the command language of the terminal debugger.

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, fixture, line_of, source};

fn repl(dap: &mut Dap, frame: &Value, line: &str) -> String {
    dap.request(
        "evaluate",
        json!({"expression": line, "frameId": frame, "context": "repl"}),
    )["result"]
        .as_str()
        .expect("result")
        .to_owned()
}

#[test]
fn console_commands_see_the_focused_frame_and_cannot_run_the_program() {
    let path = source("c/variables.c");
    let mut dap = Dap::start("console");
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
    let trace = dap.request("stackTrace", json!({"threadId": stop.thread}));
    let frames = trace["stackFrames"].as_array().expect("frames").clone();

    let backtrace = repl(&mut dap, &frames[0]["id"], "bt");
    assert!(backtrace.starts_with("#0 "), "{backtrace}");
    assert!(backtrace.contains("in pointer_target at"), "{backtrace}");
    // Commands run in the frame the client focuses.
    assert_eq!(
        repl(&mut dap, &frames[0]["id"], "print parameter"),
        "(int) parameter = 40"
    );
    assert_eq!(
        repl(&mut dap, &frames[1]["id"], "print single"),
        "(float) single = 1.25"
    );
    let source = repl(&mut dap, &frames[1]["id"], "list");
    assert!(source.contains("=> "), "{source}");
    assert!(
        source.contains(&format!(
            "{}",
            line_of(&path, "pointer_target(40, &pointer_parameter_value) +")
        )),
        "{source}"
    );
    // Lines that are no command are expressions.
    assert_eq!(dap.request("evaluate", json!({"expression": "*pointer_parameter", "frameId": frames[0]["id"], "context": "repl"}))["result"], "42");
    let signals = repl(&mut dap, &frames[0]["id"], "info signals");
    assert!(signals.contains("SIGSEGV"), "{signals}");
    for command in ["continue", "next", "step", "run", "finish", "quit"] {
        let message = dap.request_error(
            "evaluate",
            json!({"expression": command, "frameId": frames[0]["id"], "context": "repl"}),
        );
        assert_eq!(
            message,
            format!(
                "`{command}` is not available in the debug console; use the debugger's controls"
            )
        );
    }
    assert!(
        dap.request_error(
            "evaluate",
            json!({"expression": "print nope", "frameId": frames[0]["id"], "context": "repl"})
        )
        .contains("nope")
    );
    dap.finish();
}

#[test]
fn breakpoints_made_in_the_console_are_announced_and_reported() {
    let mut dap = Dap::start("console breakpoints");
    let started = dap.launch(
        Profile::Neovim,
        &fixture("basic"),
        json!({"stopOnEntry": true}),
        &Configuration::default(),
    );
    let entry = dap.stopped(started.mark);
    let mark = dap.mark();
    let output = dap.request(
        "evaluate",
        json!({"expression": "break breakpoint_target", "context": "repl"}),
    );
    assert!(
        output["result"]
            .as_str()
            .is_some_and(|text| text.starts_with("breakpoint 1 set")),
        "{output}"
    );
    let new = dap.event(mark, "breakpoint", |body| body["reason"] == "new");
    assert_eq!(new["breakpoint"]["verified"], true);
    assert_eq!(
        new["breakpoint"]["line"],
        line_of(&source("c/basic.c"), "return uscope_value;")
    );
    let id = new["breakpoint"]["id"].clone();

    let resumed = dap.send("continue", json!({"threadId": entry.thread}));
    dap.success(resumed);
    let stop = dap.stopped(resumed.mark);
    assert_eq!(stop.body["hitBreakpointIds"], json!([id]));
    let mark = dap.mark();
    dap.request(
        "evaluate",
        json!({"expression": "delete 1", "context": "repl"}),
    );
    let removed = dap.event(mark, "breakpoint", |body| body["reason"] == "removed");
    assert_eq!(removed["breakpoint"]["id"], id);
    let resumed = dap.send("continue", json!({"threadId": stop.thread}));
    dap.success(resumed);
    assert_eq!(
        dap.event(resumed.mark, "exited", |_| true),
        json!({"exitCode": 0})
    );
    dap.finish();
}

#[test]
fn console_output_is_styled_only_for_clients_that_accept_styling() {
    for ansi in [false, true] {
        let mut dap = Dap::start(format!("ansi {ansi}"));
        let mark = dap.mark();
        dap.request(
            "initialize",
            json!({"adapterID": "uscope", "supportsANSIStyling": ansi}),
        );
        dap.event(mark, "initialized", |_| true);
        let launch = dap.send(
            "launch",
            json!({"program": fixture("basic"), "stopOnEntry": true}),
        );
        dap.request("configurationDone", Value::Null);
        dap.success(launch);
        let stop = dap.stopped(launch.mark);
        let frame =
            dap.request("stackTrace", json!({"threadId": stop.thread}))["stackFrames"][0]["id"]
                .clone();
        let output = repl(&mut dap, &frame, "bt");
        assert_eq!(output.contains('\u{1b}'), ansi, "{output:?}");
        dap.finish();
    }
}
