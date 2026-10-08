//! Steps into awaits of each shape a future takes, from an async function
//! tokio runs as a task.

use std::process::Stdio;

use uscope::{BreakpointSpec, LaunchOptions, LineNumber, StepKind, StopReason};

use crate::invariants::checked;
use crate::stops::{line, place};

const BUILDS: [&str; 2] = ["tokio-shapes-o0", "tokio-shapes-o3"];
const SOURCE: &str = "shapes/src/main.rs";

/// `step` into an await stops at the first line of the future's own code
/// in one step, passing the code that builds the future and the library's
/// code that hands it on: for an async function, an async block, a boxed
/// trait object, a generic async function, and a trait's async method.
/// Into tokio's own future, it steps over tokio to the await's next line.
#[tokio::test]
async fn step_into_an_await_stops_at_the_futures_first_line() {
    for fixture in BUILDS {
        let mut scenario = checked(fixture);
        let awaits = [
            ("// INTO: plain", "plain", "// STEP: plain"),
            ("// INTO: block", "shapes::{async block#0}", "// STEP: block"),
            ("// INTO: boxed", "shapes::{async block#1}", "// STEP: boxed"),
            ("// INTO: generic", "generic", "// STEP: generic"),
            ("// INTO: method", "method", "// STEP: method"),
            ("// INTO: recv", "shapes", "// STEP: recv-after"),
        ];
        for (from, _, _) in awaits {
            scenario
                .add_breakpoint_spec(BreakpointSpec::Source {
                    path: SOURCE.into(),
                    line: LineNumber::new(line(SOURCE, from)).expect("one-based"),
                })
                .await;
        }
        let mut reason = scenario
            .run_with_to_stop(LaunchOptions {
                stdout: Some(Stdio::null()),
                ..LaunchOptions::default()
            })
            .await;
        for (from, function, to) in awaits {
            let context = format!("{fixture} {from}");
            assert!(
                matches!(reason, StopReason::Breakpoint { .. }),
                "{context}: {reason:?}"
            );
            assert_eq!(
                place(&scenario).await,
                ("shapes".to_owned(), line(SOURCE, from)),
                "{context}"
            );
            let kind = StepKind::IntoSource;
            assert_eq!(
                scenario.step_to_stop(kind).await,
                StopReason::Step { kind },
                "{context}"
            );
            assert_eq!(
                place(&scenario).await,
                (function.to_owned(), line(SOURCE, to)),
                "{context}"
            );
            if to != "// STEP: recv-after" {
                reason = scenario.resume_to_stop().await;
            }
        }
        scenario.shutdown().await;
    }
}
