use std::path::PathBuf;
use std::time::Duration;
use tokio::time::timeout;
use uscope::{
    BreakpointSpec, Debugger, DebuggerEvent, Error, ExitStatus, InferiorState, StopReason,
};

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
    let breakpoint = handle
        .add_breakpoint(BreakpointSpec::Function("breakpoint_target".into()))
        .await
        .expect("set breakpoint");

    let first = handle.run().await.expect("run to first breakpoint");
    let first_address = match first {
        StopReason::Breakpoint { address } => address,
        other => panic!("expected breakpoint, got {other:?}"),
    };
    let location = handle
        .current_location()
        .await
        .expect("resolve stop location");
    assert_eq!(location.address, first_address);
    assert_eq!(
        location
            .image
            .function
            .as_ref()
            .map(|function| function.name.as_ref()),
        Some("breakpoint_target")
    );
    let source = location.image.source.as_ref().expect("source location");
    let source_file = handle
        .module_image()
        .source_file(source.file)
        .expect("source file");
    assert!(source_file.path.ends_with("basic.c"));
    assert!(source.line.get() > 0);
    let image_breakpoint = match breakpoint {
        uscope::BreakpointLocation::Image(address) => address,
        uscope::BreakpointLocation::Virtual(_) => panic!("function breakpoint was not image-based"),
    };
    assert_ne!(
        first_address.get(),
        image_breakpoint.get(),
        "PIE was not relocated"
    );
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
    assert_eq!(snapshot.breakpoints.as_ref(), &[breakpoint]);

    let main_breakpoint = handle
        .add_breakpoint(BreakpointSpec::Function("main".into()))
        .await
        .expect("set breakpoint after launch");
    let snapshot = handle.snapshot().await.expect("updated state snapshot");
    assert_eq!(
        snapshot.breakpoints.as_ref(),
        &[breakpoint, main_breakpoint]
    );

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
        StopReason::Exited(ExitStatus::Code(0))
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
        if let DebuggerEvent::InferiorExited { status, .. } = event {
            break status;
        }
    };
    assert!(matches!(
        exited,
        ExitStatus::Terminated(exception) if exception.code == 9
    ));
    assert!(matches!(
        run.await.expect("run task"),
        Err(Error::RequestCancelled)
    ));
}

#[tokio::test]
async fn dwarf_cfi_unwinds_nested_calls_without_frame_pointers() {
    for fixture_name in ["unwind-o0", "unwind-o2", "unwind-nopie"] {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("build/test-programs")
            .join(fixture_name);
        assert!(
            fixture.exists(),
            "missing test fixture; run `just build-test-programs`"
        );

        let debugger = Debugger::new(&fixture).expect("load debugger");
        let handle = debugger.handle();
        handle
            .add_breakpoint(BreakpointSpec::Function("deepest".into()))
            .await
            .expect("set breakpoint");
        assert!(matches!(
            handle.run().await.expect("run to breakpoint"),
            StopReason::Breakpoint { .. }
        ));

        let trace = handle.backtrace().await.expect("collect backtrace");
        let names: Vec<_> = trace
            .frames
            .iter()
            .filter_map(|frame| frame.function.as_ref())
            .map(|function| function.name.as_ref())
            .collect();

        assert!(
            names.starts_with(&["deepest", "middle", "outer", "main"]),
            "unexpected {fixture_name} backtrace: {trace:?}"
        );
        assert!(
            trace.frames.len() >= 4,
            "backtrace was truncated: {trace:?}"
        );
        debugger.shutdown().await.expect("shutdown worker");
    }
}
