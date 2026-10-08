//! tokio's tasks as a client sees them: as its threads, beside the
//! program's own threads, without the runtime's idle workers.

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, fixture};

/// The workers fixture, stopped at its checkpoint, with its threads.
fn at_checkpoint(name: &str) -> (Dap, i64, Vec<(i64, String)>) {
    stopped_at_checkpoint(name, "tokio-workers-o0")
}

/// A tokio fixture, stopped at its checkpoint, with its threads.
fn stopped_at_checkpoint(name: &str, program: &str) -> (Dap, i64, Vec<(i64, String)>) {
    let mut dap = Dap::start(name);
    let started = dap.launch(
        Profile::VsCode,
        &fixture(program),
        json!({}),
        &Configuration {
            functions: vec!["truth_reached".to_owned()],
            ..Configuration::default()
        },
    );
    let stop = dap.stopped(started.mark);
    assert_eq!(stop.reason, "function breakpoint", "{stop:?}");
    let listed = dap.request("threads", Value::Null)["threads"]
        .as_array()
        .expect("threads")
        .iter()
        .map(|thread| {
            (
                thread["id"].as_i64().expect("an id"),
                thread["name"].as_str().expect("a name").to_owned(),
            )
        })
        .collect::<Vec<_>>();
    (dap, stop.thread, listed)
}

#[test]
fn tokio_tasks_are_threads_beside_the_programs_own() {
    let (dap, stopped, listed) = at_checkpoint("tokio tasks");
    // The thread that stopped runs no task, and comes first.
    let (first, name) = &listed[0];
    assert_eq!(*first, stopped, "{listed:?}");
    assert!(name.ends_with("— at breakpoint 1"), "{listed:?}");
    let named = |text: &str| {
        listed
            .iter()
            .filter(|(_, name)| name.contains(text))
            .count()
    };
    assert_eq!(named("— suspended"), 8, "{listed:?}");
    assert_eq!(named("— queued in the blocking pool"), 1, "{listed:?}");
    assert_eq!(
        named("— running a blocking closure (thread "),
        1,
        "{listed:?}"
    );
    // The process's first thread, which waits for the one that stopped, is
    // the program's own; the runtime's idle workers are left out.
    assert_eq!(listed.len(), 12, "{listed:?}");
    assert_eq!(named("tokio-rt-worker"), 0, "{listed:?}");
    dap.finish();
}

/// A suspended task's stack is its chain of awaits: the future it awaits,
/// then each async function at its await, with tokio's own subdued. An
/// async function's frame shows the locals it keeps, and offers no
/// registers, which it has none of.
#[test]
fn a_suspended_tasks_stack_is_its_chain_of_awaits() {
    let (mut dap, _, listed) = at_checkpoint("tokio awaits");
    let (task, _) = listed
        .iter()
        .find(|(_, name)| name.contains("— suspended"))
        .unwrap_or_else(|| panic!("{listed:?}"));
    let trace = dap.request("stackTrace", json!({"threadId": task}));
    let frames = trace["stackFrames"].as_array().expect("frames");
    let awaited = &frames[0];
    assert!(
        awaited["name"]
            .as_str()
            .is_some_and(|name| name.starts_with("awaiting ")),
        "{trace}"
    );
    assert!(
        awaited.get("instructionPointerReference").is_none(),
        "{trace}"
    );
    let own = frames
        .iter()
        .filter(|frame| frame.get("presentationHint").is_none())
        .map(|frame| frame["name"].as_str().expect("a name"))
        .collect::<Vec<_>>();
    assert_eq!(own, ["async leaf", "async middle", "async top"], "{trace}");

    let leaf = frames
        .iter()
        .find(|frame| frame["name"] == "async leaf")
        .expect("the leaf's frame");
    let scopes = dap.request("scopes", json!({"frameId": leaf["id"]}))["scopes"]
        .as_array()
        .expect("scopes")
        .iter()
        .map(|scope| scope["name"].as_str().expect("a name").to_owned())
        .collect::<Vec<_>>();
    assert!(
        !scopes.iter().any(|scope| scope == "Registers"),
        "{scopes:?}"
    );
    let local = dap.request(
        "evaluate",
        json!({"expression": "leaf_local", "frameId": leaf["id"], "context": "watch"}),
    );
    // Each task records its own number, `me`, in its locals.
    assert_eq!(local["result"], (task * 100 + 3).to_string(), "{local}");
    dap.finish();
}

/// A thread that blocks on a future shows the future's awaits under a
/// label, before the frame of tokio's that drives it, and each async
/// function's frame keeps its locals.
#[test]
fn a_blocked_threads_stack_holds_the_future_it_drives() {
    let (mut dap, stopped, listed) = stopped_at_checkpoint("tokio driven", "tokio-drivers-o0");
    // The main thread drives `#[tokio::main]`'s future while another
    // reaches the checkpoint.
    let (main, _) = listed
        .iter()
        .find(|(id, _)| *id != stopped)
        .unwrap_or_else(|| panic!("{listed:?}"));
    let trace = dap.request("stackTrace", json!({"threadId": main}));
    let frames = trace["stackFrames"].as_array().expect("frames");
    let names = frames
        .iter()
        .map(|frame| frame["name"].as_str().expect("a name"))
        .collect::<Vec<_>>();
    let label = names
        .iter()
        .position(|name| *name == "in the future the next frame drives")
        .unwrap_or_else(|| panic!("{trace}"));
    assert!(
        names[label + 1].starts_with("awaiting tokio::sync::oneshot::Receiver<u32>"),
        "{trace}"
    );
    assert_eq!(
        names[label + 2..label + 5],
        [
            "async waiting",
            "async driven",
            "async tokio_main::{async block#0}"
        ],
        "{trace}"
    );
    assert_eq!(names[label + 5], "on the thread's stack", "{trace}");
    let local = dap.request(
        "evaluate",
        json!({"expression": "waiting_local", "frameId": frames[label + 2]["id"], "context": "watch"}),
    );
    assert_eq!(local["result"], "7", "{local}");
    dap.finish();
}
