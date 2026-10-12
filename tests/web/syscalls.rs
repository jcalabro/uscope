//! A profile's system call trace, as the Syscalls view reads it: what each
//! call cost, when, from where, and what it did with files and processes.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::support::{Scenario, ScratchDir};
use crate::web::{Client, Web};

/// The syscalls fixture's trace in `mode`, by the ptrace engine with
/// stacks, written in `directory`, where the program runs.
fn trace(directory: &Path, mode: &str) -> PathBuf {
    let path = directory.join(format!("{mode}.usprof"));
    let status = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args(["syscalls", "--engine", "ptrace", "-f", "-k", "-o"])
        .arg(&path)
        .arg("--")
        .arg(Scenario::fixture("syscalls"))
        .arg(mode)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .expect("run uscope syscalls");
    assert!(status.code().is_some(), "{status}");
    path
}

/// The tab, and the id of the one profile its server serves.
async fn serve(web: &Web) -> (Client, String) {
    let mut tab = web.control("tab").await;
    let listed = tab
        .expect("the profiles", |message| message["type"] == "profiles")
        .await;
    let id = listed["profiles"][0]["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    (tab, id)
}

/// A line's text, its tokens joined.
fn text(line: &Value) -> String {
    line["tokens"]
        .as_array()
        .expect("tokens")
        .iter()
        .map(|token| token["text"].as_str().expect("text"))
        .collect()
}

fn sum(values: &Value) -> u64 {
    values
        .as_array()
        .expect("numbers")
        .iter()
        .map(|value| value.as_u64().expect("a count"))
        .sum()
}

/// Every view of one trace agrees with what the program did: it opened,
/// wrote, read and unlinked one file, failed to open another, and exited
/// with 3.
#[tokio::test]
async fn a_trace_is_served_as_costs_lines_callers_files_and_lanes() {
    let scratch = ScratchDir::new("web-syscalls");
    let path = trace(scratch.path(), "files");
    let web = Web::start("syscalls", &[path.to_str().expect("a path")]);
    let (mut tab, id) = serve(&web).await;

    let summary = tab
        .ok(
            "syscallSummary",
            json!({"profile": id, "filter": "trace=write,read,unlink"}),
        )
        .await;
    let rows = summary["rows"].as_array().expect("rows");
    let calls = |name: &str| {
        rows.iter()
            .find(|row| row["name"] == name)
            .map(|row| row["calls"].as_u64().expect("calls"))
    };
    assert_eq!(
        (calls("write"), calls("unlink")),
        (Some(1), Some(1)),
        "{summary}"
    );
    assert_eq!(summary["engine"], "ptrace");
    assert!(summary["total"].as_u64() > summary["calls"].as_u64());

    // The failed open, as strace prints it with -y, and expanded.
    let log = tab
        .ok("syscallLog", json!({"profile": id, "filter": "trace=openat status=failed", "offset": 0, "limit": 100}))
        .await;
    let lines = log["lines"].as_array().expect("lines");
    let missing = lines
        .iter()
        .find(|line| text(line).contains("\"missing\""))
        .unwrap_or_else(|| panic!("{log}"));
    assert_eq!(
        text(missing),
        format!(
            "openat(AT_FDCWD<{}>, \"missing\", O_RDONLY) = -1 ENOENT (No such file or directory)",
            scratch.path().display()
        )
    );
    assert_eq!(missing["failed"], true);
    let call = tab
        .ok(
            "syscallCall",
            json!({"profile": id, "event": missing["event"]}),
        )
        .await;
    let arguments: Vec<&str> = call["arguments"]
        .as_array()
        .expect("arguments")
        .iter()
        .map(|argument| argument["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(arguments[..3], ["dfd", "filename", "flags"], "{call}");
    let stack: Vec<&str> = call["stack"]
        .as_array()
        .expect("a stack")
        .iter()
        .map(|name| name.as_str().expect("a name"))
        .collect();
    assert!(
        stack.contains(&"files") && stack.contains(&"main"),
        "{call}"
    );

    // The failure is placed at the line that made it.
    let detail = tab
        .ok("syscallDetail", json!({"profile": id, "call": "openat"}))
        .await;
    assert_eq!(detail["errnos"][0]["name"], "ENOENT", "{detail}");
    let callers = detail["callers"].as_array().expect("callers");
    assert!(
        callers
            .iter()
            .any(|caller| caller["site"] == "files · syscalls.c:23" && caller["failed"] == 1),
        "{detail}"
    );
    assert_eq!(sum(&detail["whole"]), sum(&detail["selected"]));

    files_and_charts(&mut tab, &id, scratch.path()).await;

    let processes = tab.ok("syscallProcesses", json!({"profile": id})).await;
    assert_eq!(
        processes["processes"][0]["end"], "exited with 3",
        "{processes}"
    );
    let network = tab.ok("syscallNetwork", json!({"profile": id})).await;
    assert_eq!(network["endpoints"], json!([]));

    let refused = tab
        .request(
            "syscallSummary",
            json!({"profile": id, "filter": "trace=nosuchcall"}),
        )
        .await;
    assert!(refused.is_err(), "{refused:?}");
    tab.save_traffic("syscalls");
}

/// The files the program used, and every selected call charted once.
async fn files_and_charts(tab: &mut Client, id: &str, scratch: &Path) {
    let present = scratch.join("present").display().to_string();
    let files = tab.ok("syscallFiles", json!({"profile": id})).await;
    let file = |path: &str| {
        files["files"]
            .as_array()
            .expect("files")
            .iter()
            .find(|file| file["path"] == path)
            .cloned()
            .unwrap_or_else(|| panic!("{path}: {files}"))
    };
    let written = file(&present);
    assert_eq!(
        (&written["opens"], &written["written"], &written["read"]),
        (&json!(2), &json!(6), &json!(6)),
        "{written}"
    );
    let failed = file(&scratch.join("missing").display().to_string());
    assert_eq!(
        (&failed["opens"], &failed["failed"]),
        (&json!(1), &json!(1))
    );

    // Every selected call lands in the charts once.
    let selected = json!({"profile": id, "filter": "trace=%file", "buckets": 16});
    let rate = tab.ok("syscallRate", selected.clone()).await;
    let charted: u64 = rate["series"]
        .as_array()
        .expect("series")
        .iter()
        .map(|series| sum(&series["counts"]))
        .sum();
    let of_files = tab
        .ok(
            "syscallSummary",
            json!({"profile": id, "filter": "trace=%file"}),
        )
        .await;
    assert_eq!(Some(charted), of_files["calls"].as_u64(), "{rate}");
    let heat = tab.ok("syscallHeatmap", selected.clone()).await;
    assert_eq!(heat["counts"].as_array().expect("counts").len(), 16 * 40);
    assert_eq!(Some(sum(&heat["counts"])), of_files["calls"].as_u64());
    let busy = tab.ok("syscallLanes", selected).await;
    assert_eq!(busy["lanes"].as_array().expect("lanes").len(), 1, "{busy}");
    assert!(sum(&busy["lanes"][0]["busy"]) > 0);
}

/// Processes come parents first, each with its threads, calls, and end;
/// each thread has a lane.
#[tokio::test]
async fn a_familys_processes_and_threads_are_served_in_order() {
    let scratch = ScratchDir::new("web-syscalls-family");
    let path = trace(scratch.path(), "family");
    let web = Web::start("syscalls", &[path.to_str().expect("a path")]);
    let (mut tab, id) = serve(&web).await;
    let processes = tab.ok("syscallProcesses", json!({"profile": id})).await;
    let processes = processes["processes"].as_array().expect("processes");
    assert_eq!(processes.len(), 2, "{processes:?}");
    let (parent, child) = (&processes[0], &processes[1]);
    assert_eq!(child["parent"], parent["pid"]);
    assert_eq!(child["end"], "exited with 4");
    assert_eq!(parent["threads"], 2, "{parent}");
    let lanes = tab
        .ok("syscallLanes", json!({"profile": id, "buckets": 8}))
        .await;
    assert_eq!(
        lanes["lanes"].as_array().expect("lanes").len(),
        3,
        "{lanes}"
    );
}
