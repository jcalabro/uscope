//! Holding fork children for sessions of their own.

use super::*;

use uscope::{HeldChild, HeldProcess};

/// Attaches a session to a fixture that forks once released, holds the
/// child, and ends the session, returning the parent and the child.
async fn hold_one_child(name: &str) -> (support::ExternalProcess, HeldChild) {
    let mut parent = support::ExternalProcess::spawn(&Scenario::fixture("attach-fork"));
    let mut scenario = Scenario::attached(name, parent.attach().await);
    let mut held = scenario
        .operation("hold forks", scenario.handle().hold_forks())
        .await;
    // The child inherits this trap, and must lose it before it is held.
    scenario.add_breakpoint("child_work").await;
    let _running = scenario.start_resuming().await;
    parent.release();
    let child = timeout(Duration::from_secs(5), held.recv())
        .await
        .expect("a child is held in time")
        .expect("the parent forks");
    assert_eq!(child.parent(), parent.process_id());
    assert!(
        uscope::still_held(&child.process()).expect("read the child's state"),
        "the child waits, untraced, for a session"
    );
    // Only one session at a time can trace from this process.
    scenario.shutdown().await;
    (parent, child)
}

/// Attaches a session to a held child, which then answers for it.
async fn attach_held(child: HeldChild) -> Scenario {
    let held = child.process();
    let debugger = timeout(Duration::from_secs(5), Debugger::attach_held(held))
        .await
        .expect("attach in time")
        .expect("attach the held child");
    let _ = child.hand_over();
    assert!(!uscope::still_held(&held).expect("read the child's state"));
    Scenario::attached("held child", debugger)
}

/// Lets the parent exit once it has reaped its child, and returns its code,
/// which is the child's: 7 for a child that ran its work once and never
/// received SIGCONT. The parent exits only once no session runs, whose
/// waiter would reap it too.
fn child_exit(mut parent: support::ExternalProcess) -> Option<i32> {
    parent.release();
    wait_for_zombie(parent.process_id());
    parent.wait().code()
}

#[tokio::test]
async fn a_held_fork_child_is_debugged_by_a_session_of_its_own() {
    let (parent, child) = hold_one_child("held child's parent").await;
    let mut scenario = attach_held(child).await;
    // The child has not run: it reaches the breakpoint after its fork.
    scenario.add_breakpoint("child_work").await;
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    // Detached, it runs on rather than stopping again for job control.
    scenario.shutdown().await;
    assert_eq!(child_exit(parent), Some(7));
}

/// The SIGCONT that ends the hold is still queued when a session detaches
/// before the child ever ran; the program must not receive it.
#[tokio::test]
async fn a_held_fork_child_detached_at_once_runs_on_unaware() {
    let (parent, child) = hold_one_child("held child's parent").await;
    attach_held(child).await.shutdown().await;
    assert_eq!(child_exit(parent), Some(7));
}

#[tokio::test]
async fn a_held_fork_child_no_session_takes_runs_on_and_is_never_mistaken() {
    let (parent, child) = hold_one_child("released child's parent").await;
    let held = child.process();

    // A process that is no longer the one held is refused, and the held
    // child stays held.
    let stale = HeldProcess {
        start_time: held.start_time + 1,
        ..held
    };
    assert!(matches!(
        Debugger::attach_held(stale).await,
        Err(Error::HeldProcessGone(process)) if process == held.process_id.get()
    ));
    assert!(uscope::still_held(&held).expect("read the child's state"));

    // Dropping the child lets it run, with the SIGCONT a shell's `fg`
    // would send.
    drop(child);
    assert_eq!(child_exit(parent), Some(17));
    assert!(
        !uscope::release_held(&held).expect("release"),
        "a child no longer held is left alone"
    );
}
