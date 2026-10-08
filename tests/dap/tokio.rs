//! tokio's tasks as a client sees them: as its threads, beside the
//! program's own threads, without the runtime's idle workers.

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, fixture};

#[test]
fn tokio_tasks_are_threads_beside_the_programs_own() {
    let mut dap = Dap::start("tokio tasks");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("tokio-workers-o0"),
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
    // The thread that stopped runs no task, and comes first.
    let (first, name) = &listed[0];
    assert_eq!(*first, stop.thread, "{listed:?}");
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
