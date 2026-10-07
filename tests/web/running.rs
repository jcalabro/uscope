//! Running and stepping a program from the page, and the stacks, sources,
//! and breakpoints it shows along the way.

use serde_json::{Value, json};

use crate::support::Scenario;
use crate::web::Web;

fn fixture(name: &str) -> String {
    Scenario::fixture(name).display().to_string()
}

/// The fixture source file whose path ends with `name`.
fn source_named(files: &Value, name: &str) -> String {
    files["files"]
        .as_array()
        .expect("files")
        .iter()
        .filter_map(Value::as_str)
        .find(|path| path.ends_with(&format!("/{name}")))
        .unwrap_or_else(|| panic!("no {name} in {files}"))
        .to_owned()
}

#[tokio::test]
async fn a_breakpoint_set_before_running_stops_there_and_steps_walk_the_source() {
    let web = Web::start("steps", &[&fixture("basic")]);
    let mut tab = web.control("tab").await;
    tab.state("the program loaded", |state| state["session"].is_string())
        .await;

    // Sources come from the debug information before the program runs.
    let files = tab.ok("sources", json!(null)).await;
    let path = source_named(&files, "basic.c");
    let source = tab.ok("source", json!({"path": path})).await;
    assert!(
        source["text"]
            .as_str()
            .is_some_and(|text| text.contains("uint64_t first = breakpoint_target();")),
        "{source}"
    );
    let breakable = source["breakable"].as_array().expect("lines");
    assert!(breakable.contains(&json!(10)), "{source}");
    assert!(!breakable.contains(&json!(2)), "{source}");

    let id = tab
        .ok("addBreakpoint", json!({"location": format!("{path}:10")}))
        .await["id"]
        .clone();
    let set = tab
        .state("the breakpoint", |state| {
            state["breakpoints"][0]["id"] == id
        })
        .await;
    let breakpoint = &set["breakpoints"][0];
    assert_eq!(breakpoint["places"][0]["line"], 10, "{breakpoint}");
    assert_eq!(breakpoint["places"][0]["path"], path.as_str());

    tab.ok("continue", json!({})).await;
    let stopped = tab.inferior("stopped").await;
    let inferior = &stopped["inferior"];
    assert_eq!(inferior["reason"]["kind"], "breakpoint");
    assert_eq!(inferior["place"]["function"], "main");
    assert_eq!(inferior["place"]["line"], 10);
    assert_eq!(stopped["breakpoints"][0]["hits"], 1);
    let entry = stopped["stops"].as_array().expect("stops").last().cloned();
    assert_eq!(
        entry.as_ref().map(|entry| &entry["stop"]),
        Some(&inferior["stop"])
    );
    assert_eq!(
        entry.as_ref().map(|entry| &entry["by"]),
        Some(&json!("tester"))
    );

    let (stop, thread) = (inferior["stop"].clone(), inferior["thread"].clone());
    let trace = tab
        .ok("backtrace", json!({"stop": stop, "thread": thread}))
        .await;
    let top = &trace["frames"][0];
    assert_eq!(top["name"], "main");
    assert_eq!(top["source"]["path"], path.as_str());
    assert_eq!(top["source"]["line"], 10);

    // Into the call, out again, and over the next line.
    tab.ok(
        "step",
        json!({"stop": stop, "thread": thread, "kind": "into"}),
    )
    .await;
    let into = tab
        .state("a step into the call", |state| {
            state["inferior"]["state"] == "stopped" && state["inferior"]["stop"] != stop
        })
        .await;
    assert_eq!(
        into["inferior"]["place"]["function"], "breakpoint_target",
        "{into}"
    );
    let stop = into["inferior"]["stop"].clone();
    tab.ok(
        "step",
        json!({"stop": stop, "thread": thread, "kind": "out"}),
    )
    .await;
    let out = tab
        .state("a step out of it", |state| {
            state["inferior"]["state"] == "stopped" && state["inferior"]["stop"] != stop
        })
        .await;
    assert_eq!(out["inferior"]["place"]["function"], "main", "{out}");
    let stop = out["inferior"]["stop"].clone();
    tab.ok(
        "step",
        json!({"stop": stop, "thread": thread, "kind": "over"}),
    )
    .await;
    let over = tab
        .state("a step over the next line", |state| {
            state["inferior"]["state"] == "stopped" && state["inferior"]["stop"] != stop
        })
        .await;
    assert_eq!(over["inferior"]["reason"]["kind"], "step");
    assert_eq!(over["inferior"]["place"]["line"], 11, "{over}");

    // An earlier stop can no longer be stepped.
    let (kind, _) = tab
        .request(
            "step",
            json!({"stop": stop, "thread": thread, "kind": "over"}),
        )
        .await
        .expect_err("a stale step");
    assert_eq!(kind, "staleStop");

    tab.save_traffic("steps");
}

#[tokio::test]
async fn breakpoints_change_their_conditions_and_go_before_the_program_runs() {
    let web = Web::start("breakpoints", &[&fixture("basic")]);
    let mut tab = web.control("tab").await;
    tab.state("the program loaded", |state| state["session"].is_string())
        .await;
    let id = tab
        .ok("addBreakpoint", json!({"location": "breakpoint_target"}))
        .await["id"]
        .clone();
    tab.ok(
        "editBreakpoint",
        json!({"id": id, "condition": "uscope_value == 0", "hitCondition": ">=2"}),
    )
    .await;
    let edited = tab
        .state("the condition", |state| {
            state["breakpoints"][0]["condition"] == "uscope_value == 0"
        })
        .await;
    assert_eq!(edited["breakpoints"][0]["hitCondition"], ">=2");
    let (kind, message) = tab
        .request("editBreakpoint", json!({"id": id, "hitCondition": "2"}))
        .await
        .expect_err("an ambiguous hit condition");
    assert_eq!(kind, "invalid", "{message}");
    tab.ok("removeBreakpoint", json!({"id": id})).await;
    tab.state("no breakpoints", |state| {
        state["breakpoints"].as_array().is_some_and(Vec::is_empty)
    })
    .await;
    let (kind, _) = tab
        .request("addBreakpoint", json!({"location": "no_such_function"}))
        .await
        .expect_err("a function no module has");
    assert_eq!(kind, "failed");
    tab.save_traffic("breakpoints");
}

#[tokio::test]
async fn a_logpoint_writes_its_message_instead_of_stopping() {
    let web = Web::start("logpoint", &[&fixture("hit-counts-gcc-o0")]);
    let mut tab = web.control("tab").await;
    tab.state("the program loaded", |state| state["session"].is_string())
        .await;
    tab.ok(
        "addBreakpoint",
        json!({"location": "counted", "logMessage": "call {call}", "hitCondition": "%20"}),
    )
    .await;
    tab.ok("continue", json!({})).await;
    let log = tab.output_until("log", "call 40").await;
    assert_eq!(log.matches("call ").count(), 2, "{log}");
    assert!(log.contains("call 20\n"), "{log}");
    let exited = tab.inferior("exited").await;
    assert_eq!(exited["breakpoints"][0]["hits"], 40);
}

#[tokio::test]
async fn what_the_console_types_is_the_programs_input() {
    let web = Web::start("input", &["--run", &fixture("output-streams")]);
    let mut tab = web.control("tab").await;
    // The program flushes these lines, then waits to read.
    tab.output_until("stdout", "out 3").await;
    tab.ok("input", json!({"text": "y\n"})).await;
    tab.output_until("stdout", "stdin: data").await;
    tab.inferior("exited").await;

    // Each run gets its own input, which closing ends.
    tab.ok("continue", json!({})).await;
    tab.output_until("stdout", "out 3").await;
    tab.ok("input", json!({"text": "", "eof": true})).await;
    tab.output_until("stdout", "stdin: eof").await;
}

#[tokio::test]
async fn a_viewer_reads_stacks_and_sources_but_cannot_step_or_break() {
    let web = Web::start("viewer", &["--stop-at-entry", "--run", &fixture("basic")]);
    let mut owner = web.control("owner").await;
    let link = owner.ok("share", json!({"role": "view", "to": "/"})).await["url"]
        .as_str()
        .expect("a link")
        .to_owned();
    let mut viewer = web.joining("viewer", &link).await;
    let stopped = viewer.inferior("stopped").await;
    let (stop, thread) = (
        stopped["inferior"]["stop"].clone(),
        stopped["inferior"]["thread"].clone(),
    );
    let trace = viewer
        .ok("backtrace", json!({"stop": stop, "thread": thread}))
        .await;
    assert!(!trace["frames"].as_array().expect("frames").is_empty());
    let files = viewer.ok("sources", json!(null)).await;
    viewer
        .ok("source", json!({"path": source_named(&files, "basic.c")}))
        .await;
    for (method, params) in [
        (
            "step",
            json!({"stop": stop, "thread": thread, "kind": "over"}),
        ),
        ("addBreakpoint", json!({"location": "main"})),
        ("input", json!({"text": "x"})),
    ] {
        let (kind, _) = viewer
            .request(method, params)
            .await
            .expect_err("a viewer's change");
        assert_eq!(kind, "forbidden", "{method}");
    }
    // Only files the debug information names can be read.
    let (kind, _) = viewer
        .request("source", json!({"path": "/etc/passwd"}))
        .await
        .expect_err("a file no module names");
    assert_eq!(kind, "invalid");

    // Where each person looks is part of everyone's presence.
    viewer
        .ok(
            "setFocus",
            json!({"focus": {"url": "/s/x/stop/1", "label": "frame 0, main"}}),
        )
        .await;
    owner
        .expect("the viewer's focus", |message| {
            message["type"] == "presence"
                && message["people"].as_array().is_some_and(|people| {
                    people
                        .iter()
                        .any(|person| person["focus"]["label"] == "frame 0, main")
                })
        })
        .await;
}
