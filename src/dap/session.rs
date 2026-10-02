//! One client's debug session.
//!
//! The session handles the client's requests one at a time, in the order
//! they arrive, and forwards the debugger's events between them. Because a
//! request that resumes the inferior is answered before the session reads
//! the events it causes, a response always precedes the stop it leads to.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use uscope::{
    Backtrace, BreakpointSpec, Debugger, DebuggerEvent, DebuggerHandle, Error,
    ExceptionDisposition, ExitStatus, InferiorState, LaunchOptions, LineNumber, ModuleId,
    ModuleImage, ProcessId, ResumeScope, SignalPolicy, StackFrameId, StepKind, StopContext, StopId,
    StopReason, ThreadId, VariableSnapshot, VirtualAddress,
};

use super::breakpoints::{Breakpoints, Change, Entry, Group, Key, Placement, Slot, State, Want};
use super::config::{self, Configuration, Start};
use super::handles::References;
use super::output;
use super::protocol::{
    self, ErrorBody, Outgoing, SetBreakpointsArguments, SetExceptionBreakpointsArguments,
    SetFunctionBreakpointsArguments, ThreadArguments,
};
use super::signals::Selection;
use crate::cli::{Cli, LaunchSettings, Renderers};

/// How long the session waits for a program's output to drain after it
/// exits, in case a process it started still holds the output open.
const OUTPUT_DRAIN: std::time::Duration = std::time::Duration::from_secs(1);

/// Sends messages to the client through the connection's writer.
#[derive(Clone)]
pub struct Client {
    outgoing: mpsc::Sender<Outgoing>,
}

/// The connection to the client is gone.
#[derive(Debug, Clone, Copy)]
pub struct Closed;

impl Client {
    pub const fn new(outgoing: mpsc::Sender<Outgoing>) -> Self {
        Self { outgoing }
    }

    pub async fn event(&self, event: &'static str, body: Value) -> Result<(), Closed> {
        self.outgoing
            .send(Outgoing::Event { event, body })
            .await
            .map_err(|_| Closed)
    }

    async fn respond(
        &self,
        request: &Header,
        result: Result<Value, ErrorBody>,
    ) -> Result<(), Closed> {
        self.outgoing
            .send(Outgoing::Response {
                request_seq: request.seq.clone(),
                command: request.command.clone(),
                result,
            })
            .await
            .map_err(|_| Closed)
    }

    async fn important(&self, text: impl Into<String>) -> Result<(), Closed> {
        let mut text = text.into();
        text.push('\n');
        self.event("output", json!({"category": "important", "output": text}))
            .await
    }

    async fn console(&self, text: impl Into<String>) -> Result<(), Closed> {
        let mut text = text.into();
        text.push('\n');
        self.event("output", json!({"category": "console", "output": text}))
            .await
    }
}

/// What the reader hands the session.
#[derive(Debug)]
pub enum Inbound {
    Request {
        seq: Value,
        command: String,
        arguments: Value,
    },
    /// A request that could not be parsed but whose `seq` was recovered.
    Malformed {
        seq: Value,
        command: String,
        message: String,
    },
}

/// Identifies a request to answer.
#[derive(Debug, Clone)]
struct Header {
    seq: Value,
    command: String,
}

/// What the session does after answering a request.
enum After {
    /// Tell the client it may configure the session.
    Initialized,
    /// Start the target, as launch and configuration both arrived.
    Start,
    /// End the session.
    End,
}

/// What the client said it supports.
#[derive(Debug, Clone, Copy)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each is an independent capability the client reports"
)]
pub(super) struct ClientSupport {
    pub lines_start_at1: bool,
    pub variable_type: bool,
    pub memory_references: bool,
    pub ansi: bool,
}

/// The program being debugged.
struct Target {
    debugger: Option<Debugger>,
    handle: DebuggerHandle,
    start: Start,
    stop_on_entry: bool,
    source_paths: uscope::SourcePathMap,
    signals: Vec<(u64, Vec<String>)>,
    /// The policy each signal had before the session changed it.
    default_policies: HashMap<u64, SignalPolicy>,
    applied_policies: HashMap<u64, SignalPolicy>,
    console: Cli,
    images: HashMap<ModuleId, Arc<ModuleImage>>,
    /// Recorded source paths by the canonical local path they map to.
    recorded_paths: Option<HashMap<PathBuf, PathBuf>>,
    process: Option<ProcessId>,
    pumps: Vec<JoinHandle<()>>,
}

/// The stop the client was last told about.
#[derive(Debug, Clone)]
pub(super) struct Stop {
    pub id: StopId,
    pub thread: ThreadId,
    pub reason: StopReason,
}

pub struct Session {
    pub(super) client: Client,
    support: Option<ClientSupport>,
    after: Option<After>,
    configured: bool,
    /// The deferred `launch` or `attach` response.
    starting: Option<Header>,
    target: Option<Target>,
    events: Option<broadcast::Receiver<DebuggerEvent>>,
    breakpoints: Breakpoints,
    exceptions: Selection,
    pub(super) references: References,
    pub(super) stop: Option<Stop>,
    backtraces: HashMap<ThreadId, Arc<Backtrace>>,
    variables: HashMap<(ThreadId, StackFrameId), Arc<VariableSnapshot>>,
    threads: BTreeSet<ThreadId>,
    /// The execution the client last started, whose resume it already knows.
    resumed: Option<uscope::ExecutionId>,
    ended: bool,
}

impl Session {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            support: None,
            after: None,
            configured: false,
            starting: None,
            target: None,
            events: None,
            breakpoints: Breakpoints::default(),
            exceptions: Selection::default(),
            references: References::default(),
            stop: None,
            backtraces: HashMap::new(),
            variables: HashMap::new(),
            threads: BTreeSet::new(),
            resumed: None,
            ended: false,
        }
    }

    /// Serves the session until the client disconnects, the connection
    /// closes, or `shutdown` completes, which counts as a disconnect.
    pub async fn run(
        mut self,
        mut inbox: mpsc::Receiver<Inbound>,
        shutdown: impl std::future::Future<Output = ()>,
    ) {
        tokio::pin!(shutdown);
        loop {
            let input = tokio::select! {
                biased;
                () = &mut shutdown => None,
                message = inbox.recv() => message.map(Input::Message),
                event = next_event(&mut self.events) => Some(Input::Event(event)),
            };
            let result = match input {
                None => break,
                Some(Input::Message(message)) => self.message(message).await,
                Some(Input::Event(Ok(event))) => self.event(event).await,
                Some(Input::Event(Err(broadcast::error::RecvError::Lagged(_)))) => {
                    self.resync().await
                }
                Some(Input::Event(Err(broadcast::error::RecvError::Closed))) => {
                    self.events = None;
                    Ok(())
                }
            };
            if result.is_err() || self.ended {
                break;
            }
        }
        if !self.ended {
            let _ = self.end(None).await;
        }
        self.release().await;
    }

    async fn message(&mut self, message: Inbound) -> Result<(), Closed> {
        let (header, arguments) = match message {
            Inbound::Request {
                seq,
                command,
                arguments,
            } => (Header { seq, command }, arguments),
            Inbound::Malformed {
                seq,
                command,
                message,
            } => {
                return self
                    .client
                    .respond(&Header { seq, command }, Err(ErrorBody::new(message)))
                    .await;
            }
        };
        match self.dispatch(&header, arguments).await {
            Ok(Some(body)) => self.client.respond(&header, Ok(body)).await?,
            Ok(None) => {}
            Err(error) => self.client.respond(&header, Err(error)).await?,
        }
        match self.after.take() {
            Some(After::Initialized) => self.client.event("initialized", Value::Null).await,
            Some(After::Start) => self.start().await,
            Some(After::End) => {
                self.ended = true;
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Handles one request. `Ok(None)` defers the response.
    async fn dispatch(
        &mut self,
        header: &Header,
        arguments: Value,
    ) -> Result<Option<Value>, ErrorBody> {
        let command = header.command.as_str();
        if self.support.is_none() && command != "initialize" {
            return Err(ErrorBody::new(format!(
                "the client must send initialize before {command}"
            )));
        }
        let body = match command {
            "initialize" => self.initialize(arguments)?,
            "launch" => return self.launch(header, arguments).await,
            "attach" => return self.attach(header, arguments).await,
            "configurationDone" => self.configuration_done()?,
            "disconnect" => self.disconnect(arguments).await?,
            "terminate" => self.terminate().await?,
            "threads" => self.threads().await?,
            "stackTrace" => self.stack_trace(arguments).await?,
            "scopes" => self.scopes(arguments).await?,
            "variables" => self.variables_request(arguments).await?,
            "evaluate" => self.evaluate(arguments).await?,
            "exceptionInfo" => self.exception_info(arguments)?,
            "continue" => self.continue_execution(arguments).await?,
            "next" => {
                self.step(arguments, StepKind::OverSource, StepKind::OverInstruction)
                    .await?
            }
            "stepIn" => {
                self.step(arguments, StepKind::IntoSource, StepKind::Instruction)
                    .await?
            }
            "stepOut" => self.step(arguments, StepKind::Out, StepKind::Out).await?,
            "pause" => self.pause().await?,
            "setBreakpoints" => self.set_breakpoints(arguments).await?,
            "setFunctionBreakpoints" => self.set_function_breakpoints(arguments).await?,
            "setExceptionBreakpoints" => self.set_exception_breakpoints(arguments).await?,
            "setInstructionBreakpoints" => self.set_instruction_breakpoints(arguments).await?,
            "source" => {
                return Err(ErrorBody::new(
                    "source contents are not available from the debugger; open the file locally",
                ));
            }
            _ => {
                return Err(ErrorBody::new(format!(
                    "the '{command}' request is not supported"
                )));
            }
        };
        Ok(Some(body))
    }

    fn initialize(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        if self.support.is_some() {
            return Err(ErrorBody::new("the session is already initialized"));
        }
        let arguments = parse::<protocol::InitializeArguments>(arguments, "initialize arguments")?;
        self.support = Some(ClientSupport {
            lines_start_at1: arguments.lines_start_at1.unwrap_or(true),
            variable_type: arguments.supports_variable_type.unwrap_or(false),
            memory_references: arguments.supports_memory_references.unwrap_or(false),
            ansi: arguments.supports_ansi_styling.unwrap_or(false),
        });
        self.after = Some(After::Initialized);
        Ok(protocol::capabilities())
    }

    pub(super) const fn support(&self) -> ClientSupport {
        self.support.expect("requests after initialize")
    }

    async fn launch(
        &mut self,
        header: &Header,
        arguments: Value,
    ) -> Result<Option<Value>, ErrorBody> {
        self.check_unstarted()?;
        let configuration = config::launch(arguments).map_err(ErrorBody::shown)?;
        let Start::Launch(launch) = &configuration.start else {
            unreachable!("launch configurations launch");
        };
        let program = launch.program.clone();
        let debugger = tokio::task::spawn_blocking(move || Debugger::new(&program))
            .await
            .map_err(|error| ErrorBody::shown(error.to_string()))?
            .map_err(|error| {
                ErrorBody::shown(format!(
                    "failed to load {}: {error}",
                    launch.program.display()
                ))
            })?;
        self.adopt(debugger, configuration, header).await
    }

    async fn attach(
        &mut self,
        header: &Header,
        arguments: Value,
    ) -> Result<Option<Value>, ErrorBody> {
        self.check_unstarted()?;
        let configuration = config::attach(arguments).map_err(ErrorBody::shown)?;
        let debugger = match &configuration.start {
            Start::Attach {
                process,
                executable,
            } => {
                let process = *process;
                match executable {
                    Some(executable) => Debugger::attach_with_executable(process, executable).await,
                    None => Debugger::attach(process).await,
                }
                .map_err(|error| {
                    ErrorBody::shown(format!("failed to attach to process {process}: {error}"))
                })?
            }
            Start::Core(options) => {
                let options = options.clone();
                tokio::task::spawn_blocking(move || Debugger::open_core(&options))
                    .await
                    .map_err(|error| ErrorBody::shown(error.to_string()))?
                    .map_err(|error| {
                        ErrorBody::shown(format!("failed to open the core dump: {error}"))
                    })?
            }
            Start::Launch(_) => unreachable!("attach configurations attach"),
        };
        self.adopt(debugger, configuration, header).await
    }

    fn check_unstarted(&self) -> Result<(), ErrorBody> {
        if self.target.is_some() || self.starting.is_some() {
            return Err(ErrorBody::new(
                "the session already has a program; start a new session for another",
            ));
        }
        Ok(())
    }

    /// Makes a loaded debugger the session's target and defers the
    /// `launch` or `attach` response until configuration is done.
    async fn adopt(
        &mut self,
        debugger: Debugger,
        configuration: Configuration,
        header: &Header,
    ) -> Result<Option<Value>, ErrorBody> {
        let Configuration {
            start,
            stop_on_entry,
            source_paths,
            syntax,
            signals,
        } = configuration;
        let handle = debugger.handle().with_source_paths(source_paths.clone());
        self.events = Some(handle.subscribe());
        let console = Cli::new(
            handle.clone(),
            Renderers::uniform(self.support().ansi),
            syntax,
            LaunchSettings::default(),
        );
        let mut default_policies = HashMap::new();
        for code in uscope::signal_codes() {
            default_policies.insert(code, handle.signal_policy(code).await.map_err(error)?);
        }
        self.target = Some(Target {
            debugger: Some(debugger),
            handle,
            start,
            stop_on_entry,
            source_paths,
            signals,
            applied_policies: default_policies.clone(),
            default_policies,
            console,
            images: HashMap::new(),
            recorded_paths: None,
            process: None,
            pumps: Vec::new(),
        });
        self.apply_signal_policies().await?;
        if let Some(Start::Core(_)) = self.target.as_ref().map(|target| &target.start) {
            self.warn_core_modules().await.map_err(|Closed| closed())?;
        }
        self.resolve_unresolved().await.map_err(|Closed| closed())?;
        self.starting = Some(header.clone());
        if self.configured {
            self.after = Some(After::Start);
        }
        Ok(None)
    }

    async fn warn_core_modules(&self) -> Result<(), Closed> {
        let Some(core) = self.handle().and_then(DebuggerHandle::core_dump) else {
            return Ok(());
        };
        for warning in crate::cli::format::core_module_warnings(core) {
            self.client.important(format!("warning: {warning}")).await?;
        }
        Ok(())
    }

    fn configuration_done(&mut self) -> Result<Value, ErrorBody> {
        if self.configured {
            return Err(ErrorBody::new("configuration is already done"));
        }
        self.configured = true;
        if self.starting.is_some() {
            self.after = Some(After::Start);
        }
        Ok(json!({}))
    }

    /// Starts the target once both its configuration and `launch` or
    /// `attach` arrived: answers the deferred request, announces the
    /// process, and lets it run or reports its first stop.
    async fn start(&mut self) -> Result<(), Closed> {
        let Some(header) = self.starting.take() else {
            return Ok(());
        };
        let begun = self.begin().await;
        let (method, pipes) = match begun {
            Ok(begun) => begun,
            Err(error) => {
                self.client.respond(&header, Err(error)).await?;
                self.ended_target().await?;
                return Ok(());
            }
        };
        self.client.respond(&header, Ok(json!({}))).await?;
        let target = self.target.as_mut().expect("a started target");
        let snapshot = target.handle.snapshot().await.ok();
        let process = snapshot
            .as_ref()
            .and_then(|snapshot| match snapshot.inferior {
                InferiorState::Running { process_id, .. }
                | InferiorState::Stopped { process_id, .. } => Some(process_id),
                InferiorState::NotRunning => None,
            });
        let process = process.or_else(|| target.handle.core_dump().map(|core| core.process_id));
        target.process = process;
        let mut body = json!({
            "name": target.handle.executable().display().to_string(),
            "isLocalProcess": true,
            "startMethod": method,
            "pointerSize": 64,
        });
        // A core dump's process is not one on this machine.
        if let Some(process) = process.filter(|_| target.handle.core_dump().is_none()) {
            body["systemProcessId"] = process.get().into();
        }
        self.client.event("process", body).await?;
        for (read, category) in pipes {
            match output::spawn(read, category, self.client.clone()) {
                Ok(pump) => target.pumps.push(pump),
                Err(error) => {
                    self.client
                        .important(format!("cannot forward the program's {category}: {error}"))
                        .await?;
                }
            }
        }
        if let Some(snapshot) = snapshot {
            self.announce_threads(&snapshot).await?;
            if let InferiorState::Stopped {
                stop_id,
                thread_id,
                reason,
                ..
            } = snapshot.inferior
            {
                let target = self.target.as_ref().expect("a started target");
                match &target.start {
                    Start::Core(_) => self.stopped(stop_id, thread_id, reason).await?,
                    Start::Attach { .. } if target.stop_on_entry => {
                        self.stopped(stop_id, thread_id, StopReason::Entry).await?;
                    }
                    Start::Attach { .. } => {
                        let process = target.process.expect("an attached process");
                        let resumed = target
                            .handle
                            .continue_execution(
                                stop_id,
                                ResumeScope::Process(process),
                                ExceptionDisposition::Pass,
                            )
                            .await;
                        match resumed {
                            Ok(execution) => self.resumed = Some(execution),
                            Err(error) => {
                                self.client
                                    .important(format!("cannot resume the process: {error}"))
                                    .await?;
                                self.stopped(stop_id, thread_id, StopReason::Attach).await?;
                            }
                        }
                    }
                    Start::Launch(_) => {}
                }
            }
        }
        Ok(())
    }

    /// Launches the program, or readies an attached process or core dump,
    /// returning the `process` event's start method and output pipes.
    async fn begin(
        &mut self,
    ) -> Result<(&'static str, Vec<(std::os::fd::OwnedFd, &'static str)>), ErrorBody> {
        let target = self.target.as_mut().expect("a target to start");
        let Start::Launch(launch) = &target.start else {
            return Ok(("attach", Vec::new()));
        };
        let (stdout_read, stdout_write) =
            output::pipe().map_err(|error| ErrorBody::shown(error.to_string()))?;
        let (stderr_read, stderr_write) =
            output::pipe().map_err(|error| ErrorBody::shown(error.to_string()))?;
        let options = LaunchOptions {
            arguments: launch.arguments.clone(),
            environment: launch.environment.clone(),
            working_directory: launch.working_directory.clone(),
            stdin: Some(std::process::Stdio::null()),
            stdout: Some(stdout_write.into()),
            stderr: Some(stderr_write.into()),
            stop_at_entry: target.stop_on_entry,
        };
        let execution = target.handle.launch_with(options).await.map_err(|error| {
            ErrorBody::shown(format!(
                "failed to launch {}: {error}",
                launch.program.display()
            ))
        })?;
        self.resumed = Some(execution);
        Ok((
            "launch",
            vec![(stdout_read, "stdout"), (stderr_read, "stderr")],
        ))
    }

    async fn disconnect(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<protocol::DisconnectArguments>(arguments, "disconnect arguments")?;
        self.end(arguments.terminate_debuggee)
            .await
            .map_err(|Closed| closed())?;
        self.after = Some(After::End);
        Ok(json!({}))
    }

    /// Ends the session: kills a launched program, or one attached to when
    /// asked, detaches from the rest, and reports the program's end.
    async fn end(&mut self, terminate: Option<bool>) -> Result<(), Closed> {
        self.ended = true;
        let Some(target) = self.target.as_mut() else {
            return Ok(());
        };
        let launched = matches!(target.start, Start::Launch(_));
        if terminate.unwrap_or(launched)
            && target.process.is_some()
            && target.handle.core_dump().is_none()
            && let Err(error) = target.handle.kill().await
            && !matches!(error, Error::NotRunning)
        {
            self.client
                .important(format!("cannot kill the program: {error}"))
                .await?;
        }
        let shutdown = match target.debugger.take() {
            Some(debugger) => debugger.shutdown().await,
            None => Ok(()),
        };
        if let Err(error) = shutdown {
            self.client
                .important(format!("the debugger did not shut down cleanly: {error}"))
                .await?;
        }
        // Report the exit the kill caused, then the session's end.
        while let Some(event) = self
            .events
            .as_mut()
            .and_then(|events| events.try_recv().ok())
        {
            self.event(event).await?;
        }
        if self
            .target
            .as_ref()
            .is_some_and(|target| target.process.is_some())
        {
            self.ended_target().await?;
        }
        Ok(())
    }

    async fn terminate(&self) -> Result<Value, ErrorBody> {
        let Some(handle) = self.handle() else {
            return Ok(json!({}));
        };
        match handle.terminate().await {
            Ok(()) | Err(Error::NotRunning) => Ok(json!({})),
            Err(error) => Err(self::error(error)),
        }
    }

    /// Stops releasing resources once the session is over.
    async fn release(&mut self) {
        if let Some(target) = self.target.as_mut() {
            if let Some(debugger) = target.debugger.take() {
                let _ = debugger.shutdown().await;
            }
            for pump in target.pumps.drain(..) {
                pump.abort();
            }
        }
    }

    fn handle(&self) -> Option<&DebuggerHandle> {
        self.target.as_ref().map(|target| &target.handle)
    }

    /// The target's debugger, for requests that need a program.
    pub(super) fn target_handle(&self) -> Result<DebuggerHandle, ErrorBody> {
        self.handle()
            .cloned()
            .ok_or_else(|| ErrorBody::new("no program is loaded"))
    }

    /// The stop the client was told about, for requests that need one.
    pub(super) fn current_stop(&self) -> Result<Stop, ErrorBody> {
        self.stop.clone().ok_or_else(ErrorBody::not_stopped)
    }

    async fn continue_execution(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<ThreadArguments>(arguments, "continue arguments")?;
        let stop = self.current_stop()?;
        let single = arguments.single_thread.unwrap_or(false);
        let scope = self.resume_scope(single, &arguments)?;
        let handle = self.target_handle()?;
        let execution = handle
            .continue_execution(stop.id, scope, ExceptionDisposition::Pass)
            .await
            .map_err(error)?;
        self.resumed = Some(execution);
        self.leave_stop();
        Ok(json!({"allThreadsContinued": !single}))
    }

    fn resume_scope(
        &self,
        single: bool,
        arguments: &ThreadArguments,
    ) -> Result<ResumeScope, ErrorBody> {
        if single {
            return Ok(ResumeScope::Thread(thread_id(arguments.thread_id)?));
        }
        self.target
            .as_ref()
            .and_then(|target| target.process)
            .map(ResumeScope::Process)
            .ok_or_else(|| ErrorBody::new("the program is not running"))
    }

    async fn step(
        &mut self,
        arguments: Value,
        source: StepKind,
        instruction: StepKind,
    ) -> Result<Value, ErrorBody> {
        let arguments = parse::<ThreadArguments>(arguments, "step arguments")?;
        let stop = self.current_stop()?;
        let thread = thread_id(arguments.thread_id)?;
        let single = arguments.single_thread.unwrap_or(false);
        let scope = self.resume_scope(single, &arguments)?;
        let kind = if arguments.granularity.as_deref() == Some("instruction") {
            instruction
        } else {
            source
        };
        let handle = self.target_handle()?;
        let execution = handle
            .start_step(
                stop.id,
                thread,
                StackFrameId::INNERMOST,
                kind,
                scope,
                ExceptionDisposition::Pass,
            )
            .await
            .map_err(error)?;
        self.resumed = Some(execution);
        self.leave_stop();
        // Without this, clients assume only the stepping thread runs.
        self.client
            .event(
                "continued",
                json!({"threadId": arguments.thread_id, "allThreadsContinued": !single}),
            )
            .await
            .map_err(|Closed| closed())?;
        Ok(json!({}))
    }

    async fn pause(&self) -> Result<Value, ErrorBody> {
        let handle = self.target_handle()?;
        match handle.pause().await {
            Ok(_) | Err(Error::AlreadyStopped) => Ok(json!({})),
            Err(Error::NotRunning) => Err(ErrorBody::new("the program is not running")),
            Err(error) => Err(self::error(error)),
        }
    }

    /// Forgets the current stop and everything that referred to it.
    fn leave_stop(&mut self) {
        self.stop = None;
        self.references.clear();
        self.backtraces.clear();
        self.variables.clear();
    }

    async fn event(&mut self, event: DebuggerEvent) -> Result<(), Closed> {
        match event {
            DebuggerEvent::InferiorLaunched { process_id, .. } => {
                if let Some(target) = self.target.as_mut() {
                    target.process = Some(process_id);
                }
                // The first thread's id is the process id.
                self.announce_thread(ThreadId::new(process_id.get()))
                    .await?;
            }
            DebuggerEvent::InferiorAttached { process_id, .. } => {
                if let Some(target) = self.target.as_mut() {
                    target.process = Some(process_id);
                }
            }
            DebuggerEvent::InferiorContinued {
                execution_id,
                process_id,
                ..
            } => {
                if self.resumed != Some(execution_id) && self.stop.is_some() {
                    let thread = self
                        .stop
                        .as_ref()
                        .map_or_else(|| process_id.get(), |stop| stop.thread.get());
                    self.leave_stop();
                    self.client
                        .event(
                            "continued",
                            json!({"threadId": thread, "allThreadsContinued": true}),
                        )
                        .await?;
                }
            }
            DebuggerEvent::InferiorStopped {
                stop_id,
                thread_id,
                reason,
                ..
            } => self.stopped(stop_id, thread_id, reason).await?,
            DebuggerEvent::ThreadStarted { thread_id, .. } => {
                self.announce_thread(thread_id).await?;
            }
            DebuggerEvent::ThreadExited { thread_id, .. } => {
                if self.threads.remove(&thread_id) {
                    self.client
                        .event(
                            "thread",
                            json!({"reason": "exited", "threadId": thread_id.get()}),
                        )
                        .await?;
                }
            }
            DebuggerEvent::ModuleLoaded { module, .. } => {
                self.client
                    .event(
                        "module",
                        json!({"reason": "new", "module": module_json(&module)}),
                    )
                    .await?;
            }
            DebuggerEvent::ModuleUnloaded { module, .. } => {
                if let Some(target) = self.target.as_mut() {
                    target.images.remove(&module.module.id);
                }
                self.client
                    .event(
                        "module",
                        json!({"reason": "removed", "module": module_json(&module)}),
                    )
                    .await?;
            }
            DebuggerEvent::InferiorExited { status, .. } => self.exited(&status).await?,
            DebuggerEvent::InferiorDetached { .. } => {
                if let Some(target) = self.target.as_mut() {
                    target.process = None;
                }
                self.leave_stop();
                self.client.event("terminated", json!({})).await?;
            }
            DebuggerEvent::SignalReceived {
                thread_id,
                exception,
                ..
            } => {
                self.client
                    .console(crate::cli::format::signal_received(
                        thread_id,
                        &exception,
                        crate::cli::terminal::Renderer::new(false),
                    ))
                    .await?;
            }
            DebuggerEvent::BreakpointsChanged { .. } => self.sync_breakpoints().await?,
            DebuggerEvent::StateChanged { .. }
            | DebuggerEvent::WatchpointsChanged { .. }
            | DebuggerEvent::WatchpointsInvalidated { .. } => {}
        }
        Ok(())
    }

    /// Reports a stop to the client.
    async fn stopped(
        &mut self,
        stop: StopId,
        thread: ThreadId,
        reason: StopReason,
    ) -> Result<(), Closed> {
        self.leave_stop();
        self.announce_thread(thread).await?;
        let mut body = json!({
            "threadId": thread.get(),
            "allThreadsStopped": true,
            "preserveFocusHint": false,
        });
        let (kind, description, text) = match &reason {
            StopReason::Breakpoint { hits, .. } => {
                let (ids, kind) = self.breakpoints.hit(hits);
                body["hitBreakpointIds"] = ids.into();
                (kind, None, None)
            }
            StopReason::Step { .. } => ("step", None, None),
            StopReason::Pause => ("pause", None, None),
            StopReason::Entry | StopReason::Attach => ("entry", None, None),
            StopReason::Exception(info)
            | StopReason::CoreDump {
                exception: Some(info),
            } => (
                "exception",
                Some(info.description.to_string()),
                Some(signal_text(info.code)),
            ),
            StopReason::CoreDump { exception: None } => (
                "exception",
                Some("the core dump records no signal".to_owned()),
                Some("core dump".to_owned()),
            ),
            StopReason::ThreadExited { thread_id, .. } => (
                "step",
                Some(format!("thread {thread_id} exited during the step")),
                None,
            ),
            StopReason::Exec => (
                "exception",
                Some(
                    "the process replaced its executable image (exec), which is not followed"
                        .to_owned(),
                ),
                Some("exec".to_owned()),
            ),
            StopReason::Unclassifiable { description } => (
                "exception",
                Some(format!(
                    "stopped for an unclassifiable reason: {description}"
                )),
                Some("unclassifiable stop".to_owned()),
            ),
            StopReason::WatchpointArmFailed { description, .. } => (
                "exception",
                Some(format!("watchpoints could not be armed: {description}")),
                Some("watchpoint failure".to_owned()),
            ),
            StopReason::Watchpoint { .. } | StopReason::WatchpointInvalidated { .. } => {
                ("data breakpoint", None, None)
            }
            StopReason::Exited(_) => return Ok(()),
        };
        body["reason"] = kind.into();
        if let Some(description) = description {
            body["description"] = description.into();
        }
        if let Some(text) = text {
            body["text"] = text.into();
        }
        self.stop = Some(Stop {
            id: stop,
            thread,
            reason,
        });
        self.client.event("stopped", body).await
    }

    async fn exited(&mut self, status: &ExitStatus) -> Result<(), Closed> {
        if let Some(target) = self.target.as_mut() {
            // The program's last output comes before its exit.
            for mut pump in target.pumps.drain(..) {
                if tokio::time::timeout(OUTPUT_DRAIN, &mut pump).await.is_err() {
                    pump.abort();
                }
            }
        }
        self.leave_stop();
        let code = match status {
            ExitStatus::Code(code) => *code,
            ExitStatus::Terminated(info) => {
                self.client
                    .important(format!(
                        "the program was terminated by {} ({})",
                        signal_text(info.code),
                        info.description
                    ))
                    .await?;
                128 + i64::try_from(info.code).unwrap_or(0)
            }
        };
        self.client
            .event("exited", json!({"exitCode": code}))
            .await?;
        self.ended_target().await
    }

    /// Reports that the program is gone.
    async fn ended_target(&mut self) -> Result<(), Closed> {
        if let Some(target) = self.target.as_mut() {
            target.process = None;
        }
        self.threads.clear();
        self.client.event("terminated", json!({})).await
    }

    async fn announce_thread(&mut self, thread: ThreadId) -> Result<(), Closed> {
        if self.threads.insert(thread) {
            self.client
                .event(
                    "thread",
                    json!({"reason": "started", "threadId": thread.get()}),
                )
                .await?;
        }
        Ok(())
    }

    /// Announces the snapshot's threads not yet announced, and the exit of
    /// announced threads it no longer has.
    async fn announce_threads(&mut self, snapshot: &uscope::StateSnapshot) -> Result<(), Closed> {
        let live = snapshot
            .threads
            .iter()
            .map(|thread| thread.id)
            .collect::<BTreeSet<_>>();
        for gone in &self.threads - &live {
            self.threads.remove(&gone);
            self.client
                .event(
                    "thread",
                    json!({"reason": "exited", "threadId": gone.get()}),
                )
                .await?;
        }
        for thread in live {
            self.announce_thread(thread).await?;
        }
        Ok(())
    }

    /// Catches up after missing events: announces threads, and reports the
    /// current stop, resume, or exit the client has not heard about.
    async fn resync(&mut self) -> Result<(), Closed> {
        let Some(handle) = self.handle().cloned() else {
            return Ok(());
        };
        let Ok(snapshot) = handle.snapshot().await else {
            return Ok(());
        };
        self.announce_threads(&snapshot).await?;
        match snapshot.inferior {
            InferiorState::Stopped {
                stop_id,
                thread_id,
                reason,
                ..
            } => {
                if self.stop.as_ref().map(|stop| stop.id) != Some(stop_id) {
                    self.stopped(stop_id, thread_id, reason).await?;
                }
            }
            InferiorState::Running { .. } => {
                if let Some(stop) = self.stop.take() {
                    self.leave_stop();
                    self.client
                        .event(
                            "continued",
                            json!({"threadId": stop.thread.get(), "allThreadsContinued": true}),
                        )
                        .await?;
                }
            }
            InferiorState::NotRunning => {
                if self
                    .target
                    .as_ref()
                    .is_some_and(|target| target.process.is_some())
                {
                    self.leave_stop();
                    self.client
                        .important("the program ended, but its exit status was missed")
                        .await?;
                    self.ended_target().await?;
                }
            }
        }
        Ok(())
    }

    // Breakpoints.

    async fn set_breakpoints(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<SetBreakpointsArguments>(arguments, "setBreakpoints arguments")?;
        let path = arguments.source.path.clone().ok_or_else(|| {
            ErrorBody::new(
                "breakpoints need a source with a path; source references are not supported",
            )
        })?;
        let lines_start_at1 = self.support().lines_start_at1;
        let to_line = |line: i64| -> u64 {
            let line = u64::try_from(line).unwrap_or(0);
            if lines_start_at1 { line } else { line + 1 }
        };
        let wants = match (arguments.breakpoints, arguments.lines) {
            (Some(breakpoints), _) => breakpoints
                .into_iter()
                .map(|breakpoint| Want {
                    key: Key::Line(to_line(breakpoint.line)),
                    condition: breakpoint.condition.filter(|text| !text.trim().is_empty()),
                    hit_condition: breakpoint
                        .hit_condition
                        .filter(|text| !text.trim().is_empty()),
                    log_message: breakpoint.log_message,
                })
                .collect(),
            (None, Some(lines)) => lines
                .into_iter()
                .map(|line| Want::at(Key::Line(to_line(line))))
                .collect(),
            (None, None) => Vec::new(),
        };
        let group = Group::Source(PathBuf::from(&path));
        let entries = self.replace_group(group.clone(), wants).await?;
        Ok(
            json!({"breakpoints": entries.iter().map(|entry| self.breakpoint_json(&group, entry)).collect::<Vec<_>>()}),
        )
    }

    async fn set_function_breakpoints(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<SetFunctionBreakpointsArguments>(
            arguments,
            "setFunctionBreakpoints arguments",
        )?;
        let wants = arguments
            .breakpoints
            .into_iter()
            .map(|breakpoint| Want {
                key: Key::Function(breakpoint.name.trim().to_owned()),
                condition: breakpoint.condition.filter(|text| !text.trim().is_empty()),
                hit_condition: breakpoint
                    .hit_condition
                    .filter(|text| !text.trim().is_empty()),
                log_message: None,
            })
            .collect();
        let entries = self.replace_group(Group::Functions, wants).await?;
        Ok(
            json!({"breakpoints": entries.iter().map(|entry| self.breakpoint_json(&Group::Functions, entry)).collect::<Vec<_>>()}),
        )
    }

    async fn set_instruction_breakpoints(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<protocol::SetInstructionBreakpointsArguments>(
            arguments,
            "setInstructionBreakpoints arguments",
        )?;
        let mut wants = Vec::new();
        for breakpoint in arguments.breakpoints {
            let address = protocol::address(&breakpoint.instruction_reference)
                .and_then(|address| protocol::offset(address, breakpoint.offset))
                .ok_or_else(|| {
                    ErrorBody::new(format!(
                        "invalid instruction reference '{}' with offset {}",
                        breakpoint.instruction_reference,
                        breakpoint.offset.unwrap_or(0)
                    ))
                })?;
            wants.push(Want {
                key: Key::Instruction(address),
                condition: breakpoint.condition.filter(|text| !text.trim().is_empty()),
                hit_condition: breakpoint
                    .hit_condition
                    .filter(|text| !text.trim().is_empty()),
                log_message: None,
            });
        }
        let entries = self.replace_group(Group::Instructions, wants).await?;
        Ok(
            json!({"breakpoints": entries.iter().map(|entry| self.breakpoint_json(&Group::Instructions, entry)).collect::<Vec<_>>()}),
        )
    }

    /// Replaces a group of breakpoints and returns its new entries in
    /// request order.
    async fn replace_group(
        &mut self,
        group: Group,
        wants: Vec<Want>,
    ) -> Result<Vec<Entry>, ErrorBody> {
        let plan = self.breakpoints.plan(&group, wants);
        for entry in plan.release {
            self.release_breakpoint(&entry).await?;
        }
        let mut entries = Vec::with_capacity(plan.slots.len());
        for slot in plan.slots {
            entries.push(match slot {
                Slot::Keep(entry) => entry,
                Slot::Resolve { id, want } => {
                    let state = self.resolve(&group, &want).await;
                    Entry { id, want, state }
                }
            });
        }
        self.breakpoints.install(group, entries.clone());
        Ok(entries)
    }

    async fn release_breakpoint(&mut self, entry: &Entry) -> Result<(), ErrorBody> {
        let Some(breakpoint) = entry.breakpoint() else {
            return Ok(());
        };
        if self.breakpoints.release(breakpoint)
            && let Some(handle) = self.handle()
        {
            match handle.remove_breakpoint(breakpoint).await {
                Ok(_) | Err(Error::BreakpointNotFound(_)) => {}
                Err(error) => return Err(self::error(error)),
            }
        }
        Ok(())
    }

    /// Installs one breakpoint, or explains why it is not installed.
    async fn resolve(&mut self, group: &Group, want: &Want) -> State {
        let failed = |message: String| State::Unresolved {
            message,
            pending: false,
        };
        let Some(handle) = self.handle().cloned() else {
            return State::Unresolved {
                message: "the program is not loaded yet".to_owned(),
                pending: true,
            };
        };
        if want.condition.is_some() {
            return failed("conditional breakpoints are not supported yet".to_owned());
        }
        if want.log_message.is_some() {
            return failed("logpoints are not supported yet".to_owned());
        }
        let hit_condition = match want
            .hit_condition
            .as_deref()
            .map(str::parse::<uscope::HitCondition>)
        {
            None => None,
            Some(Ok(condition)) => Some(condition),
            Some(Err(error)) => return failed(error.to_string()),
        };
        let spec = match (group, &want.key) {
            (Group::Source(path), Key::Line(line)) => {
                let Some(line) = LineNumber::new(*line) else {
                    return failed("line 0 does not exist".to_owned());
                };
                BreakpointSpec::Source {
                    path: self.recorded_path(path),
                    line,
                }
            }
            (_, Key::Function(name)) => match crate::cli::commands::parse_breakpoint_location(name)
            {
                Ok(Some(spec)) => spec,
                Ok(None) => {
                    return failed(format!(
                        "'{name}' is not a function, 0xaddress, file:line, or file:function"
                    ));
                }
                Err(error) => return failed(format!("{error:#}")),
            },
            (_, Key::Instruction(address)) => {
                BreakpointSpec::Address(VirtualAddress::new(*address))
            }
            _ => return failed("this breakpoint cannot be placed".to_owned()),
        };
        let added = match hit_condition {
            Some(condition) => {
                handle
                    .add_breakpoint_with_hit_condition(spec, condition)
                    .await
            }
            None => handle.add_breakpoint(spec).await,
        };
        match added {
            Ok(breakpoint) => {
                self.breakpoints.acquire(breakpoint.id);
                State::Resolved {
                    breakpoint: breakpoint.id,
                    placement: self.placement(&breakpoint),
                }
            }
            Err(error) => failed(error.to_string()),
        }
    }

    /// Resolves breakpoints that were waiting for the program and tells
    /// the client which now are.
    async fn resolve_unresolved(&mut self) -> Result<(), Closed> {
        for (group, entry) in self.breakpoints.unresolved() {
            let state = self.resolve(&group, &entry.want).await;
            let entry = Entry { state, ..entry };
            self.breakpoints.replace(&group, entry.clone());
            self.client
                .event(
                    "breakpoint",
                    json!({"reason": "changed", "breakpoint": self.breakpoint_json(&group, &entry)}),
                )
                .await?;
        }
        Ok(())
    }

    /// Adopts breakpoints made or deleted from the console.
    async fn sync_breakpoints(&mut self) -> Result<(), Closed> {
        let Some(handle) = self.handle().cloned() else {
            return Ok(());
        };
        let Ok(snapshot) = handle.snapshot().await else {
            return Ok(());
        };
        for change in self.breakpoints.sync(&snapshot.breakpoints) {
            let (reason, mut entry) = match change {
                Change::New(entry) => ("new", entry),
                Change::Removed(entry) => ("removed", entry),
            };
            if let (
                State::Resolved {
                    breakpoint,
                    placement,
                },
                "new",
            ) = (&mut entry.state, reason)
                && let Some(core) = snapshot
                    .breakpoints
                    .iter()
                    .find(|core| core.id == *breakpoint)
            {
                *placement = self.placement(core);
                self.breakpoints.replace(&Group::Console, entry.clone());
            }
            self.client
                .event(
                    "breakpoint",
                    json!({"reason": reason, "breakpoint": self.breakpoint_json(&Group::Console, &entry)}),
                )
                .await?;
        }
        Ok(())
    }

    /// Where a debugger breakpoint resolved: its first location's source
    /// line and address.
    fn placement(&self, breakpoint: &uscope::Breakpoint) -> Placement {
        let Some(target) = &self.target else {
            return Placement::default();
        };
        let image = target.handle.module_image();
        let Some(first) = breakpoint.locations.first() else {
            return Placement::default();
        };
        match first.location {
            uscope::BreakpointLocation::Image(address) => Placement {
                source: image.source_location(address).and_then(|location| {
                    let file = image.source_file(location.file)?;
                    let line = match &breakpoint.spec {
                        // A line keeps the file the client named.
                        BreakpointSpec::Source { path, line } => image
                            .source_file_matching(path)
                            .ok()
                            .and_then(|file| image.breakpoint_line(file.id, *line))
                            .unwrap_or(location.line),
                        _ => location.line,
                    };
                    Some((self.local_path(&file.path), line.get()))
                }),
                address: None,
            },
            uscope::BreakpointLocation::Virtual(address) => Placement {
                source: None,
                address: Some(address.get()),
            },
        }
    }

    fn breakpoint_json(&self, group: &Group, entry: &Entry) -> Value {
        let to_client = |line: u64| -> u64 {
            if self.support.is_none_or(|support| support.lines_start_at1) {
                line
            } else {
                line.saturating_sub(1)
            }
        };
        let mut body = json!({"id": entry.id});
        let source = |path: &Path| {
            json!({
                "name": path.file_name().map(|name| name.to_string_lossy().into_owned()),
                "path": path.display().to_string(),
            })
        };
        match &entry.state {
            State::Resolved { placement, .. } => {
                body["verified"] = true.into();
                if let Some((path, line)) = &placement.source {
                    body["line"] = to_client(*line).into();
                    body["source"] = match group {
                        Group::Source(client) => source(client),
                        _ => source(path),
                    };
                }
                if let Some(address) = placement.address {
                    body["instructionReference"] = format!("{address:#x}").into();
                }
            }
            State::Unresolved { message, pending } => {
                body["verified"] = false.into();
                body["message"] = message.as_str().into();
                body["reason"] = if *pending { "pending" } else { "failed" }.into();
                if let (Group::Source(client), Key::Line(line)) = (group, &entry.want.key) {
                    body["line"] = to_client(*line).into();
                    body["source"] = source(client);
                }
            }
        }
        body
    }

    async fn set_exception_breakpoints(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<SetExceptionBreakpointsArguments>(
            arguments,
            "setExceptionBreakpoints arguments",
        )?;
        let (selection, breakpoints) = Selection::parse(&arguments);
        self.exceptions = selection;
        self.apply_signal_policies().await?;
        Ok(json!({"breakpoints": breakpoints}))
    }

    /// Makes the debugger stop on the signals the exception filters select,
    /// with the configuration's per-signal handling applied over them.
    async fn apply_signal_policies(&mut self) -> Result<(), ErrorBody> {
        let Some(target) = self.target.as_mut() else {
            return Ok(());
        };
        for code in uscope::signal_codes() {
            let mut policy = target.default_policies[&code];
            policy.stop = self.exceptions.stops(code);
            policy.print |= policy.stop;
            for (_, actions) in target.signals.iter().filter(|(signal, _)| *signal == code) {
                for action in actions {
                    crate::cli::commands::apply_signal_action(&mut policy, action)
                        .map_err(|error| ErrorBody::new(error.to_string()))?;
                }
            }
            if target.applied_policies.get(&code) != Some(&policy) {
                target
                    .handle
                    .set_signal_policy(code, policy)
                    .await
                    .map_err(error)?;
                target.applied_policies.insert(code, policy);
            }
        }
        Ok(())
    }

    // Paths.

    /// Where a recorded source file is on this machine: the first place
    /// the source map finds it, or its recorded path.
    pub(super) fn local_path(&self, recorded: &Path) -> PathBuf {
        let Some(target) = &self.target else {
            return recorded.to_owned();
        };
        target
            .source_paths
            .candidates(recorded)
            .into_iter()
            .find(|candidate| candidate.exists())
            .unwrap_or_else(|| recorded.to_owned())
    }

    /// The recorded path of the source file a client path names: the one
    /// whose local copy it is, or else the only one ending with the most of
    /// its trailing components.
    fn recorded_path(&mut self, client: &Path) -> PathBuf {
        let Some(target) = self.target.as_mut() else {
            return client.to_owned();
        };
        let image = Arc::clone(target.handle.module_image());
        let index = target.recorded_paths.get_or_insert_with(|| {
            let mut index = HashMap::new();
            for file in image.source_files() {
                for candidate in target.source_paths.candidates(&file.path) {
                    if let Ok(canonical) = std::fs::canonicalize(&candidate) {
                        index
                            .entry(canonical)
                            .or_insert_with(|| file.path.as_ref().clone());
                    }
                }
            }
            index
        });
        let canonical = std::fs::canonicalize(client).unwrap_or_else(|_| client.to_owned());
        if let Some(recorded) = index.get(&canonical) {
            return recorded.clone();
        }
        let components = client.components().collect::<Vec<_>>();
        for kept in (1..=components.len()).rev() {
            let suffix = components[components.len() - kept..]
                .iter()
                .collect::<PathBuf>();
            let mut matches = image
                .source_files()
                .iter()
                .filter(|file| file.path.ends_with(&suffix));
            if let (Some(only), None) = (matches.next(), matches.next()) {
                return only.path.as_ref().clone();
            }
        }
        client.to_owned()
    }

    pub(super) async fn image(&mut self, module: ModuleId) -> Option<Arc<ModuleImage>> {
        let target = self.target.as_mut()?;
        if let Some(image) = target.images.get(&module) {
            return Some(Arc::clone(image));
        }
        let image = target.handle.loaded_module_image(module).await.ok()?;
        target.images.insert(module, Arc::clone(&image));
        Some(image)
    }

    pub(super) async fn backtrace(
        &mut self,
        stop: &Stop,
        thread: ThreadId,
    ) -> Result<Arc<Backtrace>, ErrorBody> {
        if let Some(trace) = self.backtraces.get(&thread) {
            return Ok(Arc::clone(trace));
        }
        let handle = self.target_handle()?;
        let trace = Arc::new(
            handle
                .at(StopContext {
                    stop: stop.id,
                    thread,
                    frame: StackFrameId::INNERMOST,
                })
                .backtrace()
                .await
                .map_err(error)?,
        );
        self.backtraces.insert(thread, Arc::clone(&trace));
        Ok(trace)
    }

    pub(super) async fn frame_variables(
        &mut self,
        context: StopContext,
    ) -> Result<Arc<VariableSnapshot>, ErrorBody> {
        if let Some(snapshot) = self.variables.get(&(context.thread, context.frame)) {
            return Ok(Arc::clone(snapshot));
        }
        let handle = self.target_handle()?;
        let snapshot = Arc::new(handle.at(context).variables().await.map_err(error)?);
        self.variables
            .insert((context.thread, context.frame), Arc::clone(&snapshot));
        Ok(snapshot)
    }

    pub(super) fn console(&self) -> Result<&Cli, ErrorBody> {
        self.target
            .as_ref()
            .map(|target| &target.console)
            .ok_or_else(|| ErrorBody::new("no program is loaded"))
    }

    /// Converts a client line to a one-based line.
    pub(super) const fn line_to_client(&self, line: u64) -> u64 {
        if self.support().lines_start_at1 {
            line
        } else {
            line.saturating_sub(1)
        }
    }
}

enum Input {
    Message(Inbound),
    Event(Result<DebuggerEvent, broadcast::error::RecvError>),
}

async fn next_event(
    events: &mut Option<broadcast::Receiver<DebuggerEvent>>,
) -> Result<DebuggerEvent, broadcast::error::RecvError> {
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

/// Parses request arguments into their type.
pub(super) fn parse<T: serde::de::DeserializeOwned>(
    arguments: Value,
    what: &str,
) -> Result<T, ErrorBody> {
    config::parse(arguments, what).map_err(ErrorBody::new)
}

/// Describes a debugger error to the client.
pub(super) fn error(error: Error) -> ErrorBody {
    match error {
        Error::NotStopped => ErrorBody::not_stopped(),
        error => ErrorBody::new(error.to_string()),
    }
}

fn closed() -> ErrorBody {
    ErrorBody::new("the connection to the client closed")
}

/// A client's thread id as the debugger's.
pub(super) fn thread_id(id: i64) -> Result<ThreadId, ErrorBody> {
    u64::try_from(id)
        .ok()
        .filter(|id| *id != 0)
        .map(ThreadId::new)
        .ok_or_else(|| ErrorBody::new(format!("there is no thread {id}")))
}

/// A signal's name, or its number when it has none.
pub(super) fn signal_text(code: u64) -> String {
    uscope::signal_name(code).unwrap_or_else(|| format!("signal {code}"))
}

fn module_json(module: &uscope::LoadedModuleRecord) -> Value {
    json!({
        "id": module.module.id.get(),
        "name": module
            .path
            .file_name()
            .map_or_else(|| module.path.display().to_string(), |name| name.to_string_lossy().into_owned()),
        "path": module.path.display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_ids_must_be_positive() {
        assert_eq!(thread_id(7).map(ThreadId::get).ok(), Some(7));
        for invalid in [0, -1] {
            assert_eq!(
                thread_id(invalid).err().map(|error| error.format),
                Some(format!("there is no thread {invalid}"))
            );
        }
    }
}
