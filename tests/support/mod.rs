use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use uscope::{
    Breakpoint, BreakpointSpec, Debugger, DebuggerEvent, DebuggerHandle, ExceptionDisposition,
    ExitStatus, ProcessId, Result, StateSnapshot, StepKind, StopReason,
};

const OPERATION_TIMEOUT: Duration = Duration::from_secs(2);

pub struct Scenario {
    name: String,
    debugger: Option<Debugger>,
    handle: DebuggerHandle,
    events: broadcast::Receiver<DebuggerEvent>,
    transcript: Vec<String>,
    process_id: Option<ProcessId>,
    last_revision: u64,
    last_exit: Option<ExitStatus>,
}

impl Scenario {
    pub fn new(name: impl Into<String>, fixture: impl AsRef<Path>) -> Self {
        let name = name.into();
        let fixture = fixture.as_ref();
        assert!(
            fixture.exists(),
            "missing test fixture {}; run `just build-test-programs`",
            fixture.display()
        );
        let debugger = Debugger::new(fixture).expect("initialize debugger scenario");
        let handle = debugger.handle();
        let events = handle.subscribe();

        Self {
            name,
            debugger: Some(debugger),
            handle,
            events,
            transcript: Vec::new(),
            process_id: None,
            last_revision: 0,
            last_exit: None,
        }
    }

    pub fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("build/test-programs")
            .join(name)
    }

    pub const fn handle(&self) -> &DebuggerHandle {
        &self.handle
    }

    pub const fn last_revision(&self) -> u64 {
        self.last_revision
    }

    pub async fn add_breakpoint(&mut self, name: &str) -> Breakpoint {
        self.transcript.push(format!("request: break {name}"));
        let result = within(
            self.handle
                .add_breakpoint(BreakpointSpec::Function(name.to_owned())),
        )
        .await
        .unwrap_or_else(|error| self.fail(&format!("add breakpoint failed: {error}")));
        self.transcript.push(format!("reply: {result:?}"));
        self.drain_events();
        result
    }

    pub async fn run_to_stop(&mut self) -> StopReason {
        self.run_request(true).await
    }

    pub async fn resume_to_stop(&mut self) -> StopReason {
        self.run_request(false).await
    }

    pub async fn resume_with_exception(&mut self, disposition: ExceptionDisposition) -> StopReason {
        self.transcript
            .push(format!("request: continue {disposition:?}"));
        let handle = self.handle.clone();
        let task = tokio::spawn(async move { handle.resume_with_exception(disposition).await });
        self.wait_for_request(task, "continue").await
    }

    pub async fn step_to_stop(&mut self, kind: StepKind) -> StopReason {
        self.transcript.push(format!("request: step {kind:?}"));
        let handle = self.handle.clone();
        let task = tokio::spawn(async move { handle.step(kind).await });
        self.wait_for_request(task, "step").await
    }

    async fn run_request(&mut self, launch: bool) -> StopReason {
        let operation = if launch { "run" } else { "continue" };
        self.transcript.push(format!("request: {operation}"));
        let handle = self.handle.clone();
        let task = tokio::spawn(async move {
            if launch {
                handle.run().await
            } else {
                handle.resume().await
            }
        });
        self.wait_for_request(task, operation).await
    }

    async fn wait_for_request(
        &mut self,
        task: JoinHandle<Result<StopReason>>,
        operation: &str,
    ) -> StopReason {
        let event = self
            .wait_for(|event| {
                matches!(
                    event,
                    DebuggerEvent::InferiorStopped { .. } | DebuggerEvent::InferiorExited { .. }
                )
            })
            .await;
        let reply = join_request(task).await.unwrap_or_else(|error| {
            self.fail(&format!("{operation} request failed: {error}"));
        });
        self.transcript.push(format!("reply: {reply:?}"));
        self.assert_terminal_event(&event, &reply);
        reply
    }

    pub async fn start_running(&mut self) -> JoinHandle<Result<StopReason>> {
        self.transcript.push("request: run".to_owned());
        let handle = self.handle.clone();
        let task = tokio::spawn(async move { handle.run().await });
        self.wait_for(|event| matches!(event, DebuggerEvent::InferiorLaunched { .. }))
            .await;
        task
    }

    pub async fn snapshot(&mut self) -> StateSnapshot {
        let snapshot = within(self.handle.snapshot())
            .await
            .unwrap_or_else(|error| self.fail(&format!("snapshot failed: {error}")));
        self.transcript.push(format!("snapshot: {snapshot:?}"));
        snapshot
    }

    pub fn drain_pending_events(&mut self) {
        self.drain_events();
    }

    pub async fn operation<T>(&self, name: &str, future: impl Future<Output = Result<T>>) -> T {
        within(future)
            .await
            .unwrap_or_else(|error| self.fail(&format!("{name} failed: {error}")))
    }

    pub async fn shutdown(mut self) -> Option<ExitStatus> {
        let debugger = self.debugger.take().expect("scenario owns debugger");
        within(debugger.shutdown())
            .await
            .unwrap_or_else(|error| self.fail(&format!("shutdown failed: {error}")));
        self.drain_events();
        self.assert_reaped();
        self.last_exit
    }

    async fn wait_for(&mut self, predicate: impl Fn(&DebuggerEvent) -> bool) -> DebuggerEvent {
        loop {
            let event = timeout(OPERATION_TIMEOUT, self.events.recv())
                .await
                .unwrap_or_else(|_| self.fail("event timeout"))
                .unwrap_or_else(|error| self.fail(&format!("event stream failed: {error}")));
            self.record_event(&event);

            if predicate(&event) {
                return event;
            }
        }
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.record_event(&event);
        }
    }

    fn record_event(&mut self, event: &DebuggerEvent) {
        self.transcript.push(format!("event: {event:?}"));

        let revision = match event {
            DebuggerEvent::StateChanged { revision }
            | DebuggerEvent::BreakpointsChanged { revision }
            | DebuggerEvent::InferiorLaunched { revision, .. }
            | DebuggerEvent::InferiorContinued { revision, .. }
            | DebuggerEvent::InferiorStopped { revision, .. }
            | DebuggerEvent::ThreadStarted { revision, .. }
            | DebuggerEvent::ThreadExited { revision, .. }
            | DebuggerEvent::InferiorExited { revision, .. } => *revision,
        };
        if revision < self.last_revision {
            self.fail(&format!(
                "event revision moved backward from {} to {revision}",
                self.last_revision
            ));
        }
        self.last_revision = revision;
        if let DebuggerEvent::InferiorLaunched { process_id, .. } = event {
            self.process_id = Some(*process_id);
        }
        if let DebuggerEvent::InferiorExited { status, .. } = event {
            self.last_exit = Some(status.clone());
        }
    }

    fn assert_terminal_event(&self, event: &DebuggerEvent, reply: &StopReason) {
        let matches = match (event, reply) {
            (DebuggerEvent::InferiorStopped { reason, .. }, reply) => reason == reply,
            (DebuggerEvent::InferiorExited { status, .. }, StopReason::Exited(reply_status)) => {
                status == reply_status
            }
            _ => false,
        };

        if !matches {
            self.fail(&format!("event {event:?} did not match reply {reply:?}"));
        }
    }

    fn assert_reaped(&self) {
        if let Some(process_id) = self.process_id {
            let path = PathBuf::from(format!("/proc/{process_id}"));
            if path.exists() {
                self.fail(&format!("inferior {process_id} was not reaped"));
            }
        }
    }

    fn fail(&self, message: &str) -> ! {
        panic!(
            "scenario '{}' failed: {message}\ntranscript:\n{}",
            self.name,
            self.transcript.join("\n")
        )
    }
}

async fn within<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    timeout(OPERATION_TIMEOUT, future)
        .await
        .expect("debugger operation timed out")
}

async fn join_request(task: JoinHandle<Result<StopReason>>) -> Result<StopReason> {
    timeout(OPERATION_TIMEOUT, task)
        .await
        .expect("debugger task timed out")
        .expect("debugger task panicked")
}
