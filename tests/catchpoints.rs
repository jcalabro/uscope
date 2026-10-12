//! System-call catchpoints: stopping at a call's entry and exit with its
//! arguments, result, and path; conditions that read them; every call of
//! threads that call at once counted; and catching from a running program.

mod support;

use std::sync::Arc;

use support::Scenario;
use uscope::syscalls::{Abi, SyscallId};
use uscope::{
    CatchPhase, Catchpoint, Condition, ConditionOwner, DebuggerEvent, ExitStatus, LaunchOptions,
    ScalarValue, StopReason, SyscallStop, VariableState, VariableValue,
};

/// The program across a compiler and position-independence pair: the
/// calls go through libc's wrappers either way.
const FIXTURES: [&str; 2] = ["catch-gcc-o0", "catch-clang-o2-nopie"];

fn call(name: &str) -> SyscallId {
    SyscallId::named(Abi::X86_64, name).expect("a call x86-64 names")
}

async fn catch(
    scenario: &Scenario,
    calls: &[&str],
    phase: CatchPhase,
    condition: Option<&str>,
) -> Catchpoint {
    let calls: Arc<[SyscallId]> = calls.iter().map(|name| call(name)).collect();
    let condition = condition.map(|text| Condition::parse(text).expect("test condition"));
    scenario
        .operation(
            "catch syscall",
            scenario.handle().add_catchpoint(calls, phase, condition),
        )
        .await
}

fn caught(reason: &StopReason) -> &SyscallStop {
    match reason {
        StopReason::Syscall(stop) => stop,
        other => panic!("expected a catchpoint's stop, got {other:?}"),
    }
}

/// Whether a boolean expression holds in the selected frame.
async fn holds(scenario: &Scenario, text: &str) -> bool {
    let expression = uscope::Expression::parse(text).expect("test expression");
    match scenario
        .operation(text, scenario.handle().evaluate(&expression))
        .await
    {
        uscope::Evaluation::Value { value, .. } => match value.state {
            VariableState::Available {
                value: VariableValue::Scalar(ScalarValue::Boolean(holds)),
                ..
            } => holds,
            state => panic!("`{text}` has no truth: {state:?}"),
        },
        other => panic!("`{text}` is not a value: {other:?}"),
    }
}

async fn hit_count(scenario: &mut Scenario, catchpoint: &Catchpoint) -> u64 {
    scenario
        .snapshot()
        .await
        .catchpoints
        .iter()
        .find(|listed| listed.id == catchpoint.id)
        .expect("the catchpoint is listed")
        .hit_count
}

#[tokio::test]
async fn a_catchpoint_stops_at_a_calls_entry_and_exit_with_what_it_passed_and_returned() {
    for fixture in FIXTURES {
        let mut scenario = Scenario::launch(fixture);
        let openat = catch(&scenario, &["openat"], CatchPhase::Both, None).await;

        // The dynamic loader opens libraries first; each of its calls stops
        // at its entry and its exit.
        let mut entry = scenario.run_to_stop().await;
        while caught(&entry).path.as_deref() != Some("/nonexistent/uscope-catch") {
            entry = scenario.resume_to_stop().await;
        }
        let before = hit_count(&mut scenario, &openat).await;
        let stop = caught(&entry);
        assert_eq!(
            (stop.call, stop.returned, stop.catchpoints.as_ref()),
            (call("openat"), None, [openat.id].as_slice()),
            "{fixture}"
        );
        assert_eq!(stop.path.as_deref(), Some("/nonexistent/uscope-catch"));
        assert_eq!(stop.args[0].cast_signed(), -100, "AT_FDCWD");
        assert!(holds(&scenario, r#"$path == "/nonexistent/uscope-catch""#).await);
        assert!(holds(&scenario, "(int)$arg0 == -100").await);

        let exit = scenario.resume_to_stop().await;
        let stop = caught(&exit);
        assert_eq!(stop.returned, Some(-2), "ENOENT, {fixture}");
        assert_eq!(
            stop.args[0].cast_signed(),
            -100,
            "the exit keeps the arguments"
        );
        assert!(holds(&scenario, "$ret == -2").await);
        assert_eq!(hit_count(&mut scenario, &openat).await, before + 1);

        scenario
            .operation(
                "remove the catchpoint",
                scenario.handle().remove_catchpoint(openat.id),
            )
            .await;
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture}"
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_condition_reads_the_call_and_one_it_cannot_judge_stops() {
    let mut scenario = Scenario::launch(FIXTURES[0]);
    // Only the write to stdout stops; the one to stderr is counted.
    let writes = catch(&scenario, &["write"], CatchPhase::Entry, Some("$arg0 == 1")).await;
    let stop = scenario.run_to_stop().await;
    assert_eq!(caught(&stop).args[0], 1);
    assert_eq!(hit_count(&mut scenario, &writes).await, 2);
    scenario
        .operation(
            "remove the catchpoint",
            scenario.handle().remove_catchpoint(writes.id),
        )
        .await;
    scenario.shutdown().await;

    // A call that names no path has no `$path`, so the condition fails and
    // the call stops, as a breakpoint's unjudgeable condition does.
    let mut scenario = Scenario::launch(FIXTURES[0]);
    let failing = catch(
        &scenario,
        &["write"],
        CatchPhase::Entry,
        Some("$path == \"x\""),
    )
    .await;
    let mut events = scenario.handle().subscribe();
    let stop = scenario.run_to_stop().await;
    assert_eq!(caught(&stop).args[0], 2, "the first write stops");
    let failures: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            DebuggerEvent::ConditionFailed { owner, error, .. } => Some((owner, error)),
            _ => None,
        })
        .collect();
    assert_eq!(
        failures,
        [(
            ConditionOwner::Catchpoint(failing.id),
            Arc::from("the call names no path uscope could read")
        )]
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn threads_calling_at_once_have_every_call_counted_and_run_to_their_end() {
    let mut scenario = Scenario::launch(FIXTURES[0]);
    // A condition that never holds makes every call an internal stop.
    let getppid = catch(
        &scenario,
        &["getppid"],
        CatchPhase::Both,
        Some("$arg0 == 12345"),
    )
    .await;
    let options = LaunchOptions {
        arguments: vec!["threads".into()],
        ..LaunchOptions::default()
    };
    assert_eq!(
        scenario.run_with_to_stop(options).await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    // Four threads, a thousand calls each, entry and exit.
    assert_eq!(hit_count(&mut scenario, &getppid).await, 8000);
    scenario.shutdown().await;
}

#[tokio::test]
async fn a_catchpoint_added_while_the_program_runs_stops_its_next_call() {
    let mut scenario = Scenario::launch("catch-spin");
    let running = scenario.start_running().await;
    let getppid = catch(&scenario, &["getppid"], CatchPhase::Entry, None).await;
    let stop = running.await.expect("the run task").expect("the run");
    let stop = caught(&stop);
    assert_eq!(
        (stop.call, stop.catchpoints.as_ref()),
        (call("getppid"), [getppid.id].as_slice())
    );
    scenario.shutdown().await;
}
