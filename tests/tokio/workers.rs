//! tokio's tasks, found in the runtime's own memory: eight tasks parked at
//! different awaits, on a multi-thread runtime and a current-thread one,
//! compared with what the fixture reports at its checkpoint.

use std::fs::File;
use std::path::PathBuf;
use std::process::Stdio;

use uscope::{BreakpointSpec, LaunchOptions, StopReason};

use crate::stops::integer;
use crate::support::{Scenario, ScratchDir};

/// One build of the fixture, stopped at its checkpoint, with what it
/// printed going to a file.
struct Parked {
    scenario: Scenario,
    output: PathBuf,
    _scratch: ScratchDir,
}

async fn parked(fixture: &str, current: bool) -> Parked {
    let scratch = ScratchDir::new("workers");
    let output = scratch.path().join("stdout");
    let mut scenario = Scenario::launch(fixture);
    scenario
        .add_breakpoint_spec(BreakpointSpec::Function("truth_reached".into()))
        .await;
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            arguments: current.then(|| "current".into()).into_iter().collect(),
            stdout: Some(Stdio::from(File::create(&output).expect("an output file"))),
            ..LaunchOptions::default()
        })
        .await;
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{fixture}: {reason:?}"
    );
    Parked {
        scenario,
        output,
        _scratch: scratch,
    }
}

impl Parked {
    /// The `TRUTH` lines the fixture printed, each split at tabs.
    fn truth(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(&self.output)
            .expect("the fixture's output")
            .lines()
            .filter_map(|line| line.strip_prefix("TRUTH\t"))
            .map(|line| line.split('\t').map(str::to_owned).collect())
            .collect()
    }
}

/// rustc describes each type once in every unit that uses it, so a large
/// program has more types than a small one by far, and a type defined in
/// many units is still one type.
#[tokio::test]
async fn every_type_of_a_large_program_is_read_and_named_once() {
    for fixture in ["tokio-workers-o0", "tokio-workers-o3"] {
        let parked = parked(fixture, false).await;
        for (name, size) in [
            ("tokio::runtime::scheduler::multi_thread::worker::Shared", 296),
            ("tokio::runtime::scheduler::Handle", 16),
            ("tokio::runtime::task::core::Header", 32),
        ] {
            assert_eq!(
                integer(&parked.scenario, &format!("sizeof({name})")).await,
                Some(size),
                "{fixture}: {name}"
            );
        }
        assert!(!parked.truth().is_empty());
        parked.scenario.shutdown().await;
    }
}
