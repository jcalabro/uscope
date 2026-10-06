//! The shared harness for debugger scenarios.
//!
//! A [`Scenario`] drives the public request and event API, records every
//! request, reply, and event in a transcript that it prints on failure,
//! bounds every wait with a deadline, checks that event revisions never move
//! backward, and verifies at shutdown that the inferior was reaped. A test
//! that fails keeps the debugger's [`flight_recordings`].

#![allow(dead_code, reason = "each test crate uses a subset of the harness")]

pub mod flight_recordings;
mod memory_cap;

use std::future::Future;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus as ProcessExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nix::sys::personality::{self, Persona};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use uscope::{
    Breakpoint, BreakpointId, BreakpointSpec, CoreDumpOptions, Debugger, DebuggerEvent,
    DebuggerHandle, ExceptionDisposition, ExecutionId, ExitStatus, LaunchOptions, LineNumber,
    ProcessId, Result, ResumeScope, StackFrameId, StateSnapshot, StepKind, StopReason, ThreadId,
    VirtualAddress,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
// Event delivery depends on waiter and controller OS threads being scheduled.
const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

/// A uniquely named temporary directory, removed with its contents on drop,
/// even when the test panics.
pub struct ScratchDir(PathBuf);

impl ScratchDir {
    pub fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "uscope-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create scratch directory");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A fixture process for attach tests, killed and reaped when dropped, even
/// when the test panics.
pub struct ExternalProcess {
    child: Option<Child>,
    ready: String,
}

impl ExternalProcess {
    /// Spawns a fixture and waits for the line it prints, beginning with
    /// `READY`, once it can be attached to.
    pub fn spawn(path: &Path) -> Self {
        Self::handshake(&mut Command::new(path))
    }

    /// Spawns a shell that execs `program` with `arguments` once a line
    /// arrives on its standard input, as a terminal's launcher would.
    pub fn exec_gate(program: &Path, arguments: &[&str]) -> Self {
        Self::handshake(
            Command::new("sh")
                .args(["-c", r#"echo READY; read -r line; exec "$@""#, "sh"])
                .arg(program)
                .args(arguments),
        )
    }

    fn handshake(command: &mut Command) -> Self {
        let child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("spawn {command:?}: {error}"));
        // Owned before reading, so a failed handshake still kills the child.
        let mut process = Self {
            child: Some(child),
            ready: String::new(),
        };
        let stdout = process
            .child
            .as_mut()
            .and_then(|child| child.stdout.as_mut());
        let mut ready = String::new();
        BufReader::new(stdout.expect("fixture stdout"))
            .read_line(&mut ready)
            .expect("read fixture readiness");
        assert!(ready.starts_with("READY"), "unexpected readiness {ready:?}");
        process.ready = ready.trim_end().to_owned();
        process
    }

    /// Spawns a dynamically linked fixture that runs without a readiness
    /// handshake, and waits until its exec has finished.
    pub fn spawn_running(path: &Path) -> Self {
        let child = Command::new(path)
            .spawn()
            .unwrap_or_else(|error| panic!("spawn {}: {error}", path.display()));
        let process = Self {
            child: Some(child),
            ready: String::new(),
        };
        // `posix_spawn` returns once the exec releases the test's memory,
        // before the exec finishes, so an attach could find the test's own
        // executable or stop at the exec. Nothing shows the exec's end but
        // the new image running: its loader mapping the C library, which the
        // test's image had mapped too until the fixture's replaced it.
        let image = std::fs::canonicalize(path).expect("canonical fixture path");
        let pid = process.process_id().get();
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while std::fs::read_link(format!("/proc/{pid}/exe")).ok().as_ref() != Some(&image)
            || !std::fs::read_to_string(format!("/proc/{pid}/maps"))
                .is_ok_and(|maps| maps.contains("/libc.so"))
        {
            assert!(
                Instant::now() < deadline,
                "{} did not start",
                path.display()
            );
            std::thread::yield_now();
        }
        process
    }

    /// Returns the readiness line, without its newline.
    pub fn ready_line(&self) -> &str {
        &self.ready
    }

    pub fn process_id(&self) -> ProcessId {
        ProcessId::new(u64::from(self.child.as_ref().expect("live child").id()))
    }

    /// Attaches a debugger to the process within the event deadline.
    pub async fn attach(&self) -> Debugger {
        timeout(EVENT_TIMEOUT, Debugger::attach(self.process_id()))
            .await
            .expect("attach timed out")
            .expect("attach debugger")
    }

    /// Writes the byte a READY fixture waits for before continuing.
    pub fn release(&mut self) {
        self.child
            .as_mut()
            .expect("live child")
            .stdin
            .as_mut()
            .expect("fixture stdin")
            .write_all(b"x")
            .expect("release fixture");
    }

    /// Takes the standard input that releases the process.
    pub fn take_stdin(&mut self) -> ChildStdin {
        self.child
            .as_mut()
            .and_then(|child| child.stdin.take())
            .expect("fixture stdin")
    }

    /// Gives up a process that a debugger traced to its exit, and so reaped:
    /// the debugger runs in this process, its parent.
    pub fn reaped(mut self) {
        drop(self.child.take());
    }

    pub fn wait(mut self) -> ProcessExitStatus {
        self.child
            .take()
            .expect("live child")
            .wait()
            .expect("reap fixture")
    }
}

impl Drop for ExternalProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

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
        Self::from_debugger(name, debugger)
    }

    /// Launches the named fixture under a scenario of the same name.
    pub fn launch(fixture: &str) -> Self {
        Self::new(fixture, Self::fixture(fixture))
    }

    /// Opens a post-mortem core dump through the public API.
    pub fn open_core(name: impl Into<String>, options: &CoreDumpOptions) -> Self {
        let name = name.into();
        assert!(
            options.core.exists(),
            "missing core fixture {}; run `just build-test-programs`",
            options.core.display()
        );
        let debugger = Debugger::open_core(options).unwrap_or_else(|error| {
            panic!(
                "scenario '{name}' could not open {}: {error}",
                options.core.display()
            )
        });
        Self::from_debugger(name, debugger)
    }

    /// Drives a debugger attached to an external process. Shutting the
    /// scenario down detaches; the process's owner reaps it.
    pub fn attached(name: impl Into<String>, debugger: Debugger) -> Self {
        Self::from_debugger(name.into(), debugger)
    }

    fn from_debugger(name: String, debugger: Debugger) -> Self {
        let handle = debugger.handle();
        let events = handle.subscribe();
        flight_recordings::watch_scenarios();

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
        self.add_breakpoint_spec(BreakpointSpec::Function(name.to_owned()))
            .await
    }

    pub async fn add_source_breakpoint(&mut self, path: &str, line: u64) -> Breakpoint {
        self.add_breakpoint_spec(BreakpointSpec::Source {
            path: PathBuf::from(path),
            line: LineNumber::new(line).expect("scenario source line is one-based"),
        })
        .await
    }

    pub async fn add_file_function_breakpoint(&mut self, path: &str, function: &str) -> Breakpoint {
        self.add_breakpoint_spec(BreakpointSpec::FileFunction {
            path: PathBuf::from(path),
            function: function.to_owned(),
        })
        .await
    }

    pub async fn add_breakpoint_spec(&mut self, spec: BreakpointSpec) -> Breakpoint {
        let description = format!("{spec:?}");
        self.transcript
            .push(format!("request: break {description}"));
        let result = self
            .operation("add breakpoint", self.handle.add_breakpoint(spec))
            .await;
        self.transcript.push(format!("reply: {result:?}"));
        self.drain_events();
        result
    }

    pub async fn remove_breakpoint(&mut self, id: BreakpointId) -> Breakpoint {
        self.transcript.push(format!("request: delete {id}"));
        let result = self
            .operation("remove breakpoint", self.handle.remove_breakpoint(id))
            .await;
        self.transcript.push(format!("reply: {result:?}"));
        self.drain_events();
        result
    }

    pub async fn remove_all_breakpoints(&mut self) -> Vec<Breakpoint> {
        self.transcript.push("request: delete all".to_owned());
        let result = self
            .operation(
                "remove all breakpoints",
                self.handle.remove_all_breakpoints(),
            )
            .await;
        self.transcript.push(format!("reply: {result:?}"));
        self.drain_events();
        result.to_vec()
    }

    pub async fn run_to_stop(&mut self) -> StopReason {
        self.run_request(true).await
    }

    /// Launches with explicit options and waits for that launch's stop or exit.
    pub async fn run_with_to_stop(&mut self, options: LaunchOptions) -> StopReason {
        let task = self.spawn_request(&format!("run {options:?}"), |handle| async move {
            handle.run_with(options).await
        });
        self.wait_for_request(task, "run").await
    }

    pub async fn resume_to_stop(&mut self) -> StopReason {
        self.run_request(false).await
    }

    /// Launches the inferior the first time and resumes it afterwards.
    pub async fn resume_or_run(&mut self) -> StopReason {
        self.run_request(self.process_id.is_none()).await
    }

    pub async fn resume_with_exception(&mut self, disposition: ExceptionDisposition) -> StopReason {
        let task = self.spawn_request(
            &format!("continue {disposition:?}"),
            move |handle| async move { handle.resume_with_exception(disposition).await },
        );
        self.wait_for_request(task, "continue").await
    }

    pub async fn step_to_stop(&mut self, kind: StepKind) -> StopReason {
        let task = self.spawn_request(&format!("step {kind:?}"), move |handle| async move {
            handle.step(kind).await
        });
        self.wait_for_request(task, "step").await
    }

    /// Steps the selected thread while every other thread stays stopped.
    pub async fn step_alone_to_stop(&mut self, kind: StepKind) -> StopReason {
        let task = self.spawn_request(&format!("step {kind:?} alone"), move |handle| async move {
            let snapshot = handle.snapshot().await?;
            let (Some(stop), Some(thread), Some(frame)) = (
                snapshot.stop_id,
                snapshot.selected_thread,
                snapshot.selected_frame,
            ) else {
                return Err(uscope::Error::NotStopped);
            };
            let frame = if kind == StepKind::Out {
                frame
            } else {
                StackFrameId::INNERMOST
            };
            let mut events = handle.subscribe();
            let execution = handle
                .start_step(
                    stop,
                    thread,
                    frame,
                    kind,
                    ResumeScope::Thread(thread),
                    ExceptionDisposition::Pass,
                )
                .await?;
            Ok(execution_end(&mut events, execution).await)
        });
        self.wait_for_request(task, "step").await
    }

    /// Continues `thread` while every other thread stays stopped, until its
    /// execution stops or ends.
    pub async fn continue_alone_to_stop(&mut self, thread: ThreadId) -> StopReason {
        let task = self.spawn_request(
            &format!("continue thread {thread} alone"),
            move |handle| async move {
                let stop = handle
                    .snapshot()
                    .await?
                    .stop_id
                    .ok_or(uscope::Error::NotStopped)?;
                let mut events = handle.subscribe();
                let execution = handle
                    .continue_execution(
                        stop,
                        ResumeScope::Thread(thread),
                        ExceptionDisposition::Pass,
                    )
                    .await?;
                Ok(execution_end(&mut events, execution).await)
            },
        );
        self.wait_for_request(task, "continue").await
    }

    /// Launches through `process`, which execs the scenario's program once
    /// `release` runs, and waits for that launch's stop or exit.
    pub async fn launch_by_exec_to_stop(
        &mut self,
        process: ProcessId,
        stop_at_entry: bool,
        release: impl FnOnce() + Send + 'static,
    ) -> StopReason {
        let task = self.spawn_request(
            &format!("launch by exec of {process}, stop at entry {stop_at_entry}"),
            move |handle| async move {
                let mut events = handle.subscribe();
                let execution = handle
                    .launch_by_exec(process, stop_at_entry, release)
                    .await?;
                Ok(execution_end(&mut events, execution).await)
            },
        );
        self.wait_for_request(task, "launch by exec").await
    }

    /// Launches and returns once the program runs past its launch, so that
    /// an immediate shutdown meets an active execution rather than the
    /// launch itself.
    pub async fn start_running(&mut self) -> JoinHandle<Result<StopReason>> {
        let task = self.run_task(true);
        self.wait_for(|event| matches!(event, DebuggerEvent::InferiorContinued { .. }))
            .await;
        task
    }

    /// Launches and returns as soon as the inferior exists, which may be
    /// before its initial exec stop has been processed.
    pub async fn start_launching(&mut self) -> JoinHandle<Result<StopReason>> {
        let task = self.run_task(true);
        self.wait_for(|event| matches!(event, DebuggerEvent::InferiorLaunched { .. }))
            .await;
        task
    }

    pub async fn start_resuming(&mut self) -> JoinHandle<Result<StopReason>> {
        let task = self.run_task(false);
        self.wait_for(|event| matches!(event, DebuggerEvent::InferiorContinued { .. }))
            .await;
        task
    }

    async fn run_request(&mut self, launch: bool) -> StopReason {
        let task = self.run_task(launch);
        self.wait_for_request(task, if launch { "run" } else { "continue" })
            .await
    }

    /// Spawns a launch, or a resume.
    fn run_task(&mut self, launch: bool) -> JoinHandle<Result<StopReason>> {
        let description = if launch { "run" } else { "continue" };
        self.spawn_request(description, move |handle| async move {
            if launch {
                handle.run().await
            } else {
                handle.resume().await
            }
        })
    }

    /// Records and spawns a run-control request after the events before it.
    fn spawn_request<F>(
        &mut self,
        description: &str,
        request: impl FnOnce(DebuggerHandle) -> F,
    ) -> JoinHandle<Result<StopReason>>
    where
        F: Future<Output = Result<StopReason>> + Send + 'static,
    {
        self.drain_events();
        self.transcript.push(format!("request: {description}"));
        tokio::spawn(request(self.handle.clone()))
    }

    /// Waits for a run-control request's stop or exit. A request that fails
    /// before running reports its error rather than an event timeout.
    async fn wait_for_request(
        &mut self,
        mut task: JoinHandle<Result<StopReason>>,
        operation: &str,
    ) -> StopReason {
        let is_terminal = |event: &DebuggerEvent| {
            matches!(
                event,
                DebuggerEvent::InferiorStopped { .. } | DebuggerEvent::InferiorExited { .. }
            )
        };
        let (event, reply) = tokio::select! {
            event = self.wait_for(is_terminal) => (event, join_request(task).await),
            joined = &mut task => {
                let reply = joined.expect("debugger task panicked");
                if let Err(error) = &reply {
                    self.fail(&format!("{operation} request failed: {error}"));
                }
                (self.wait_for(is_terminal).await, reply)
            }
        };
        let reply = reply
            .unwrap_or_else(|error| self.fail(&format!("{operation} request failed: {error}")));
        self.transcript.push(format!("reply: {reply:?}"));
        self.assert_terminal_event(&event, &reply);
        reply
    }

    pub async fn snapshot(&mut self) -> StateSnapshot {
        let snapshot = self.operation("snapshot", self.handle.snapshot()).await;
        self.transcript.push(format!("snapshot: {snapshot:?}"));
        snapshot
    }

    pub fn drain_pending_events(&mut self) {
        self.drain_events();
    }

    /// Runs one request that must succeed within the request deadline.
    pub async fn operation<T>(&self, name: &str, future: impl Future<Output = Result<T>>) -> T {
        self.attempt(name, future)
            .await
            .unwrap_or_else(|error| self.fail(&format!("{name} failed: {error}")))
    }

    /// Runs one request that may fail, within the request deadline.
    pub async fn attempt<T>(
        &self,
        name: &str,
        future: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        timeout(REQUEST_TIMEOUT, future)
            .await
            .unwrap_or_else(|_| self.fail(&format!("{name} timed out")))
    }

    pub async fn shutdown(mut self) -> Option<ExitStatus> {
        let debugger = self.debugger.take().expect("scenario owns debugger");
        self.operation("shutdown", debugger.shutdown()).await;
        self.drain_events();
        self.assert_reaped();
        self.last_exit
    }

    async fn wait_for(&mut self, predicate: impl Fn(&DebuggerEvent) -> bool) -> DebuggerEvent {
        loop {
            let event = timeout(EVENT_TIMEOUT, self.events.recv())
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
        loop {
            match self.events.try_recv() {
                Ok(event) => self.record_event(&event),
                Err(broadcast::error::TryRecvError::Lagged(count)) => {
                    self.transcript
                        .push(format!("transcript skipped {count} lagged events"));
                }
                Err(_) => return,
            }
        }
    }

    fn record_event(&mut self, event: &DebuggerEvent) {
        self.transcript.push(format!("event: {event:?}"));

        let revision = event.revision();
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

/// Waits until `condition`, a fact outside the debugger such as `/proc`,
/// holds, failing with `what` at the event deadline.
pub fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Waits until `process` is blocked in system call `number`.
pub fn wait_for_system_call(process: ProcessId, number: u64) {
    let path = format!("/proc/{process}/syscall");
    wait_until(&format!("{process} is in system call {number}"), || {
        let text =
            std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {path}: {error}"));
        text.split_whitespace().next() == Some(&number.to_string())
    });
}

/// Returns the address of a breakpoint stop, failing on any other stop.
pub fn breakpoint_address(reason: &StopReason) -> VirtualAddress {
    match reason {
        StopReason::Breakpoint { address, .. } => *address,
        other => panic!("expected a breakpoint stop, got {other:?}"),
    }
}

/// Randomizes the address space of every process this thread starts from
/// now on, as an ordinary shell does; `nix develop` turns that off.
pub fn randomize_addresses() {
    let persona = personality::get().expect("read this thread's personality");
    personality::set(persona - Persona::ADDR_NO_RANDOMIZE).expect("randomize addresses");
}

async fn join_request(task: JoinHandle<Result<StopReason>>) -> Result<StopReason> {
    timeout(REQUEST_TIMEOUT, task)
        .await
        .expect("debugger task timed out")
        .expect("debugger task panicked")
}

/// Waits for the stop or exit that ends `execution`.
async fn execution_end(
    events: &mut broadcast::Receiver<DebuggerEvent>,
    execution: ExecutionId,
) -> StopReason {
    loop {
        match events.recv().await {
            Ok(DebuggerEvent::InferiorStopped {
                execution_id: Some(id),
                reason,
                ..
            }) if id == execution => return reason,
            Ok(DebuggerEvent::InferiorExited {
                execution_id: Some(id),
                status,
                ..
            }) if id == execution => return StopReason::Exited(status),
            Ok(_) => {}
            Err(error) => panic!("event stream failed: {error}"),
        }
    }
}
