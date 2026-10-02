//! Attaching to running processes and detaching without harm.

use super::*;

#[tokio::test]
async fn attach_discovers_the_executable_and_detaches_without_harming_the_process() {
    let fixture = Scenario::fixture("attach");
    let mut child = support::ExternalProcess::spawn(&fixture);

    let process = child.process_id();
    let debugger = timeout(Duration::from_secs(5), Debugger::attach(process))
        .await
        .expect("attach timed out")
        .expect("attach debugger");
    let handle = debugger.handle();
    assert_eq!(handle.executable(), fixture);
    let snapshot = handle.snapshot().await.expect("attached snapshot");
    assert!(matches!(
        snapshot.inferior,
        InferiorState::Stopped {
            process_id,
            reason: StopReason::Attach,
            ..
        } if process_id == process
    ));

    handle
        .add_breakpoint(uscope::BreakpointSpec::Function(
            "attach_breakpoint".to_owned(),
        ))
        .await
        .expect("set attached breakpoint");
    child.release();
    assert!(matches!(
        timeout(Duration::from_secs(5), handle.resume())
            .await
            .expect("attached resume timed out")
            .expect("resume attached process"),
        StopReason::Breakpoint { .. }
    ));

    debugger.shutdown().await.expect("detach debugger");
    let status = child.wait();
    assert_eq!(status.code(), Some(23));
}

#[tokio::test]
async fn attached_group_stops_are_classified_and_resumable() {
    let mut child = support::ExternalProcess::spawn(&Scenario::fixture("attach"));
    let process = child.process_id();
    let pid = Pid::from_raw(i32::try_from(process.get()).expect("PID fits i32"));
    let debugger = timeout(Duration::from_secs(5), Debugger::attach(process))
        .await
        .expect("attach timed out")
        .expect("attach debugger");
    let handle = debugger.handle();
    let resume = || async {
        timeout(Duration::from_secs(5), handle.resume())
            .await
            .expect("resume timed out")
            .expect("resume attached process")
    };

    // SIGSTOP is reported once on delivery and again as the seized
    // thread's group-stop; neither may become an unresumable stop.
    let stopping = tokio::spawn({
        let handle = handle.clone();
        async move { handle.resume().await }
    });
    while !matches!(
        handle.snapshot().await.expect("running snapshot").inferior,
        InferiorState::Running { .. }
    ) {
        tokio::task::yield_now().await;
    }
    kill(pid, Signal::SIGSTOP).expect("stop attached process");
    assert!(matches!(
        timeout(Duration::from_secs(5), stopping)
            .await
            .expect("signal stop timed out")
            .expect("join resume")
            .expect("resume attached process"),
        StopReason::Exception(exception) if exception.code == 19
    ));
    assert!(matches!(
        resume().await,
        StopReason::Exception(exception) if exception.code == 19
    ));

    debugger
        .shutdown()
        .await
        .expect("detach group-stopped process");
    kill(pid, Signal::SIGCONT).expect("continue detached process");
    child.release();
    assert_eq!(child.wait().code(), Some(23));
}

#[tokio::test]
async fn attach_stops_and_detaches_every_existing_native_thread() {
    let mut child = support::ExternalProcess::spawn(&Scenario::fixture("attach-threads"));
    let debugger = Debugger::attach(child.process_id())
        .await
        .expect("attach multithreaded fixture");
    let snapshot = debugger
        .handle()
        .snapshot()
        .await
        .expect("attached snapshot");
    assert_eq!(snapshot.threads.len(), 3);
    assert!(
        snapshot
            .threads
            .iter()
            .all(|thread| { matches!(thread.state, ThreadState::Stopped { .. }) })
    );

    child.release();
    debugger.shutdown().await.expect("detach every thread");
    assert_eq!(child.wait().code(), Some(0));
}

#[tokio::test]
async fn attaching_while_threads_are_created_traces_every_thread() {
    let child = support::ExternalProcess::spawn_running(&Scenario::fixture("attach-clones"));
    let tasks = format!("/proc/{}/task", child.process_id().get());
    for _ in 0..20 {
        let scenario = Scenario::attached("attach while cloning", child.attach().await);
        // With every thread stopped none is inside clone, so the list is
        // complete. An untraced thread would run through breakpoints and
        // die of their traps.
        let untraced = fs::read_dir(&tasks)
            .expect("list threads")
            .filter_map(|entry| {
                let path = entry.expect("thread entry").path();
                // A thread may exit while the list is read.
                let status = fs::read_to_string(path.join("status")).ok()?;
                // The tracer is the debugger's controller thread.
                let tracer = status
                    .lines()
                    .find_map(|line| line.strip_prefix("TracerPid:"))
                    .expect("status names the tracer")
                    .trim();
                let ours = tracer != "0"
                    && fs::exists(format!("/proc/self/task/{tracer}")).is_ok_and(|exists| exists);
                (!ours).then_some(path)
            })
            .collect::<Vec<_>>();
        assert!(untraced.is_empty(), "untraced threads: {untraced:?}");
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn shutdown_detaches_a_running_attached_process_and_cancels_waiters() {
    let child = support::ExternalProcess::spawn_running(&Scenario::fixture("spin"));
    let pid = child.process_id().get();
    let debugger = child.attach().await;
    let handle = debugger.handle();
    let attached = handle.snapshot().await.expect("attached snapshot").inferior;
    assert!(
        matches!(
            attached,
            InferiorState::Stopped {
                reason: StopReason::Attach,
                ..
            }
        ),
        "{attached:?}"
    );

    // A resume waiting for a stop that shutdown preempts must not hang.
    let resuming = tokio::spawn({
        let handle = handle.clone();
        async move { handle.resume().await }
    });
    while !matches!(
        handle.snapshot().await.expect("running snapshot").inferior,
        InferiorState::Running { .. }
    ) {
        tokio::task::yield_now().await;
    }
    debugger.shutdown().await.expect("detach running target");
    assert!(matches!(
        timeout(Duration::from_secs(5), resuming)
            .await
            .expect("resume waiter outlived the session")
            .expect("join resume waiter"),
        Err(Error::RequestCancelled)
    ));

    let status = fs::read_to_string(format!("/proc/{pid}/status")).expect("read target status");
    assert!(status.contains("\nTracerPid:\t0\n"), "{status}");
    assert!(
        status
            .lines()
            .any(|line| line.starts_with("State:") && !line.contains("stopped")),
        "the detached target must keep running: {status}"
    );
}

#[tokio::test]
async fn attach_reads_an_unlinked_executable_through_proc() {
    let directory = support::ScratchDir::new("attach-unlinked");
    let executable = directory.path().join("deleted-fixture");
    fs::copy(Scenario::fixture("attach"), &executable).expect("copy attach fixture");
    let mut child = support::ExternalProcess::spawn(&executable);
    fs::remove_file(&executable).expect("unlink running fixture");

    let debugger = Debugger::attach(child.process_id())
        .await
        .expect("attach through proc executable link");
    assert!(
        debugger
            .handle()
            .executable()
            .to_string_lossy()
            .ends_with(" (deleted)")
    );
    child.release();
    debugger.shutdown().await.expect("detach deleted fixture");
    assert_eq!(child.wait().code(), Some(23));
}

#[tokio::test]
async fn invalid_or_missing_attach_targets_fail_without_starting_a_session() {
    assert!(matches!(
        Debugger::attach(ProcessId::new(0)).await,
        Err(Error::InvalidProcessId(0))
    ));
    assert!(matches!(
        Debugger::attach(ProcessId::new(i32::MAX as u64)).await,
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound
    ));

    let debugger = Debugger::new(Scenario::fixture("basic"))
        .expect("failed attach did not retain the Linux session lease");
    debugger
        .shutdown()
        .await
        .expect("shutdown replacement debugger");
}
