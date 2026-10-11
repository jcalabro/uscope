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
