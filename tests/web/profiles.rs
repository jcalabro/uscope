//! Profiles a server serves, whatever it debugs: what one holds, its call
//! tree's columns, its functions, and its timeline, as the page reads them.

use std::process::{Command, Stdio};

use serde_json::{Value, json};
use uscope::profiler::{RecordOptions, SampleSource, record};

use crate::support::{Scenario, ScratchDir};
use crate::web::{Client, Web};

/// A profile of each of the six calls of `leaf` in the stacks fixture:
/// three on the main thread and three on its worker.
fn record_calls(scratch: &ScratchDir) -> std::path::PathBuf {
    let mut command = Command::new(Scenario::fixture("profile-stacks-gcc-o0"));
    command.arg("1").stdout(Stdio::null());
    let options = RecordOptions {
        source: SampleSource::Function {
            symbol: "leaf".into(),
        },
        ..RecordOptions::default()
    };
    let recording = record(command, &options).expect("record");
    let path = scratch.path().join("calls.usprof");
    recording.profile.write(&path).expect("write the profile");
    path
}

/// A call tree's nodes, from its answer and the columns before it: each
/// node's path of names from the root, own weight, and total weight.
fn nodes(tree: &Value, bytes: &[u8]) -> Vec<(Vec<String>, u64, u64)> {
    let count = usize::try_from(tree["nodes"].as_u64().expect("a count")).expect("a count");
    assert_eq!(bytes.len(), count * 24, "four columns of {count} nodes");
    let f64s = |column: usize| {
        bytes[column * count * 8..][..count * 8]
            .as_chunks::<8>()
            .0
            .iter()
            .map(|word| f64::from_le_bytes(*word))
            .collect::<Vec<_>>()
    };
    let u32s = |offset: usize| {
        bytes[offset..][..count * 4]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|word| u32::from_le_bytes(*word) as usize)
            .collect::<Vec<_>>()
    };
    let (own, total) = (f64s(0), f64s(1));
    let (keys, depths) = (u32s(count * 16), u32s(count * 20));
    let mut path: Vec<String> = Vec::new();
    (0..count)
        .map(|node| {
            path.truncate(depths[node]);
            path.push(
                tree["keys"][keys[node]]["name"]
                    .as_str()
                    .expect("a name")
                    .to_owned(),
            );
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "weights are whole"
            )]
            let weight = |weight: f64| weight as u64;
            (path.clone(), weight(own[node]), weight(total[node]))
        })
        .collect()
}

async fn tree(tab: &mut Client, query: Value) -> Vec<(Vec<String>, u64, u64)> {
    let (answer, bytes) = tab
        .with_bytes("profileTree", query)
        .await
        .unwrap_or_else(|error| panic!("a tree: {error:?}"));
    nodes(&answer, &bytes)
}

#[tokio::test]
async fn a_profile_is_served_as_trees_functions_and_a_timeline() {
    let scratch = ScratchDir::new("web-profile");
    let path = record_calls(&scratch);
    let web = Web::start("profile", &[path.to_str().expect("a path")]);
    let mut tab = web.control("tab").await;

    // The server serves the profile, and its link opens it.
    let listed = tab
        .expect("the profiles", |message| message["type"] == "profiles")
        .await;
    let id = listed["profiles"][0]["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    assert_eq!(listed["profiles"][0]["name"], "calls.usprof");
    assert_eq!(
        web.link_to.as_deref(),
        Some(format!("/profile/{id}").as_str())
    );

    let summary = tab.ok("profile", json!({"profile": id})).await;
    assert_eq!(summary["samples"], 6);
    assert_eq!(summary["unit"], "executions");
    let threads = summary["threads"].as_array().expect("threads");
    assert_eq!(threads.len(), 2, "{summary}");
    assert!(threads.iter().all(|thread| thread["samples"] == 3));

    // Every call ends in leaf, under main's descent or the worker's.
    let everything = tree(&mut tab, json!({"profile": id})).await;
    let leaves: Vec<_> = everything.iter().filter(|(_, own, _)| *own > 0).collect();
    assert!(
        leaves
            .iter()
            .all(|(path, _, _)| path.last().is_some_and(|name| name == "leaf"))
    );
    assert_eq!(leaves.iter().map(|(_, own, _)| own).sum::<u64>(), 6);
    let roots: u64 = everything
        .iter()
        .filter(|(path, _, _)| path.len() == 1)
        .map(|(_, _, total)| total)
        .sum();
    assert_eq!(roots, 6);

    // Focused on middle and inverted: middle's callers, from it outward.
    let callers = tree(
        &mut tab,
        json!({"profile": id, "invert": true, "transforms": [{"op": "focus", "name": "middle"}]}),
    )
    .await;
    assert_eq!(callers[0], (vec!["middle".to_owned()], 0, 6));
    assert_eq!(callers[1].0, ["middle", "outer"]);
    // One thread's samples only.
    let worker = threads
        .iter()
        .find(|thread| thread["tid"] != thread["pid"])
        .expect("the worker")["row"]
        .clone();
    let one_thread = tree(&mut tab, json!({"profile": id, "threads": [worker]})).await;
    assert!(
        one_thread
            .iter()
            .any(|(path, _, _)| path.iter().any(|name| name == "worker"))
    );
    assert!(
        one_thread
            .iter()
            .all(|(path, _, _)| !path.iter().any(|name| name == "main"))
    );

    let functions = tab.ok("profileFunctions", json!({"profile": id})).await;
    assert_eq!(functions["weight"], 6);
    let first = &functions["rows"][0];
    assert_eq!(
        functions["keys"]
            [usize::try_from(first["key"].as_u64().expect("a key")).expect("an index")]["name"],
        "leaf"
    );
    assert_eq!(
        (first["own"].clone(), first["total"].clone()),
        (json!(6), json!(6))
    );

    // Each sample falls in one slice of one thread's lane.
    let timeline = tab
        .ok("profileTimeline", json!({"profile": id, "buckets": 10}))
        .await;
    let lanes = timeline["lanes"].as_array().expect("lanes");
    assert_eq!(lanes.len(), 2);
    let weights: u64 = lanes
        .iter()
        .flat_map(|lane| lane["weights"].as_array().expect("weights").iter())
        .map(|weight| weight.as_u64().expect("a weight"))
        .sum();
    assert_eq!(weights, 6);

    // A transform names something the profile has, or fails.
    let (kind, message) = tab
        .with_bytes(
            "profileTree",
            json!({"profile": id, "transforms": [{"op": "merge", "name": "nothing"}]}),
        )
        .await
        .expect_err("an unknown name");
    assert_eq!(kind, "invalid");
    assert!(message.contains("nothing"), "{message}");
    tab.save_traffic("profile");
}

/// Waits for the profiles list to show `id` as `matches` accepts.
async fn listed(tab: &mut Client, id: &str, what: &str, matches: impl Fn(&Value) -> bool) -> Value {
    loop {
        let message = tab
            .expect(what, |message| message["type"] == "profiles")
            .await;
        if let Some(info) = message["profiles"]
            .as_array()
            .and_then(|profiles| profiles.iter().find(|info| info["id"] == id))
            && matches(info)
        {
            return info.clone();
        }
    }
}

#[tokio::test]
async fn a_recording_grows_live_stops_when_asked_and_downloads_whole() {
    let web = Web::start("record", &[]);
    let mut tab = web.control("tab").await;
    let started = tab
        .ok(
            "recordProfile",
            json!({"program": Scenario::fixture("spin"), "preset": "development"}),
        )
        .await;
    let id = started["profile"].as_str().expect("an id").to_owned();

    // The profile is published as it grows, each time a new revision.
    let mut samples = 0;
    let mut revision = 0;
    while samples == 0 {
        let info = listed(&mut tab, &id, "a newer revision", |info| {
            info["revision"].as_u64() > Some(revision)
        })
        .await;
        assert_eq!(info["recording"], true);
        revision = info["revision"].as_u64().expect("a revision");
        let summary = tab
            .ok("profile", json!({"profile": id, "revision": revision}))
            .await;
        samples = summary["samples"].as_u64().expect("samples");
    }
    let summary = tab.ok("profile", json!({"profile": id})).await;
    let pid: u64 = summary["settings"]
        .as_array()
        .and_then(|settings| settings.iter().find(|setting| setting["name"] == "pid"))
        .and_then(|setting| setting["value"].as_str()?.parse().ok())
        .expect("the program's pid");

    // A viewer may look but not stop it.
    let link = tab.ok("share", json!({"role": "view", "to": "/"})).await;
    let mut viewer = web
        .joining("viewer", link["url"].as_str().expect("a link"))
        .await;
    let (kind, _) = viewer
        .request("stopRecording", json!({"profile": id}))
        .await
        .expect_err("a viewer's stop");
    assert_eq!(kind, "forbidden");

    tab.ok("stopRecording", json!({"profile": id})).await;
    let ended = listed(&mut tab, &id, "the recording's end", |info| {
        info["recording"] == false
    })
    .await;
    assert!(
        ended["ended"]
            .as_str()
            .is_some_and(|ended| ended.contains("SIGKILL")),
        "{ended}"
    );
    crate::web::wait_gone(pid);

    // The file the page saves is the whole profile.
    let summary = tab.ok("profile", json!({"profile": id})).await;
    let cookie = web.cookie(&web.control_token.clone());
    let (head, body) = web.http_bytes(&format!(
        "POST /api/profile/{id} HTTP/1.1\r\nHost: {}\r\nOrigin: {}\r\nCookie: {cookie}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        web.host(),
        web.origin()
    ));
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.contains("filename=\"spin.usprof\""), "{head}");
    let saved = uscope::profiler::Profile::from_bytes(&body).expect("a profile");
    assert_eq!(
        Some(saved.profile.samples() as u64),
        summary["samples"].as_u64()
    );
}

#[tokio::test]
async fn a_server_that_ends_ends_its_recordings_programs() {
    let mut web = Web::start("record-shutdown", &[]);
    let mut tab = web.control("tab").await;
    let started = tab
        .ok(
            "recordProfile",
            json!({"program": Scenario::fixture("spin")}),
        )
        .await;
    let id = started["profile"].as_str().expect("an id");
    let summary = tab.ok("profile", json!({"profile": id})).await;
    let pid: u64 = summary["settings"]
        .as_array()
        .and_then(|settings| settings.iter().find(|setting| setting["name"] == "pid"))
        .and_then(|setting| setting["value"].as_str()?.parse().ok())
        .expect("the program's pid");
    assert!(crate::web::exists(pid));
    let status = web.interrupt();
    assert!(status.success(), "{status}");
    crate::web::wait_gone(pid);
}

#[tokio::test]
async fn a_program_that_cannot_start_fails_its_recording() {
    let web = Web::start("record-missing", &[]);
    let mut tab = web.control("tab").await;
    let (kind, message) = tab
        .request("recordProfile", json!({"program": "/nonexistent/program"}))
        .await
        .expect_err("a missing program");
    assert_eq!(kind, "failed");
    assert!(message.contains("starting the program failed"), "{message}");
}
