use std::path::PathBuf;
use std::time::Duration;
use tokio::time::timeout;
use uscope::{BreakpointSpec, Debugger, DebuggerEvent, Error, InferiorState, StopReason};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("build/test-programs/basic")
}

#[tokio::test]
async fn breakpoint_is_reinserted_and_inferior_memory_can_be_read() {
    let fixture = fixture();
    assert!(
        fixture.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let debugger = Debugger::new(&fixture).expect("load debugger");
    let handle = debugger.handle();
    let mut events = handle.subscribe();
    handle
        .add_breakpoint(BreakpointSpec::Function("breakpoint_target".into()))
        .await
        .expect("set breakpoint");

    let first = handle.run().await.expect("run to first breakpoint");
    let first_address = match first {
        StopReason::Breakpoint { address } => address,
        other => panic!("expected breakpoint, got {other:?}"),
    };
    let mut launched = false;
    let mut last_revision = 0;
    let stopped = loop {
        let event = timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("event timeout")
            .expect("event stream");

        match event {
            DebuggerEvent::StateChanged { revision } => last_revision = revision,
            DebuggerEvent::InferiorLaunched { .. } => launched = true,
            DebuggerEvent::InferiorStopped { reason, .. } => break reason,
            _ => {}
        }
    };
    assert!(launched);
    assert_eq!(stopped, first);

    let snapshot = handle.snapshot().await.expect("state snapshot");
    assert_eq!(snapshot.revision, last_revision);
    assert!(matches!(
        snapshot.inferior,
        InferiorState::Stopped { reason, .. } if reason == first
    ));
    assert_eq!(snapshot.breakpoints.as_ref(), &[first_address]);

    let value_address = handle
        .runtime_address("uscope_value")
        .await
        .expect("resolve global");
    assert_eq!(
        handle
            .read_word(value_address)
            .await
            .expect("read inferior memory"),
        0x1122_3344_5566_7788
    );

    assert_eq!(
        handle.resume().await.expect("run to second breakpoint"),
        StopReason::Breakpoint {
            address: first_address
        }
    );
    assert_eq!(
        handle.resume().await.expect("finish inferior"),
        StopReason::Exited(0)
    );
    debugger.shutdown().await.expect("shutdown worker");
}

#[tokio::test]
async fn shutdown_interrupts_and_reaps_a_running_inferior() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("build/test-programs/spin");
    assert!(
        fixture.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let debugger = Debugger::new(&fixture).expect("load debugger");
    let handle = debugger.handle();
    let mut events = handle.subscribe();
    let run = tokio::spawn({
        let handle = handle.clone();
        async move { handle.run().await }
    });

    loop {
        let event = timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("event timeout")
            .expect("event stream");
        if matches!(event, DebuggerEvent::InferiorLaunched { .. }) {
            break;
        }
    }

    let snapshot = timeout(Duration::from_secs(1), handle.snapshot())
        .await
        .expect("snapshot timeout")
        .expect("state snapshot");
    assert!(matches!(snapshot.inferior, InferiorState::Running { .. }));

    debugger.shutdown().await.expect("shutdown worker");

    let exited = loop {
        let event = timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("event timeout")
            .expect("event stream");
        if let DebuggerEvent::InferiorExited { reason, .. } = event {
            break reason;
        }
    };
    assert_eq!(exited, StopReason::Signaled(9));
    assert!(matches!(
        run.await.expect("run task"),
        Err(Error::RequestCancelled)
    ));
}
