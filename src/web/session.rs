//! The one debugging session a `uscope web` server holds, shared by every
//! connected tab.
//!
//! The session owns at most one [`Debugger`] at a time. Choosing something
//! else to debug ends the current one first: a launched program is killed
//! and an attached one detached, as [`Debugger::shutdown`] does. Each
//! debugger gets a new session id, so links to an earlier one say it ended
//! instead of opening something else.
//!
//! State is pushed: a task per debugger turns its events into whole
//! [`State`]s published through a watch channel, so a burst of events sends
//! only the newest state, and a lagged event stream costs nothing but a
//! fresh snapshot.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::AsyncReadExt as _;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use uscope::{
    CoreDumpOptions, Debugger, DebuggerEvent, DebuggerHandle, ExceptionDisposition, InferiorState,
    LaunchOptions, ProcessId, ResumeScope, StopId, StopReason,
};

use super::auth::{Tokens, random_hex};
use super::picker;
use super::protocol::{
    self, ErrorBody, ErrorKind, Inferior, Output, PathCompletions, Person, Presence, Processes,
    Request, Role, ServerMessage, ShareLink, State, Stream, TargetKind, Thread,
};
use crate::cli::format;
use crate::cli::terminal::Renderer;
use crate::dap::output;

/// The most program output kept for tabs that join later.
const OUTPUT_HISTORY: usize = 256 * 1024;

/// The most bytes one `output` message carries.
const OUTPUT_CHUNK: usize = 32 * 1024;

/// How a launched program starts.
#[derive(Debug, Clone, Default)]
pub struct LaunchSpec {
    pub program: PathBuf,
    pub arguments: Vec<OsString>,
    pub environment: Vec<(OsString, OsString)>,
    pub cwd: Option<PathBuf>,
    pub stop_at_entry: bool,
}

/// What to debug.
#[derive(Debug, Clone)]
pub enum Start {
    Launch { spec: LaunchSpec, run: bool },
    Attach(ProcessId),
    Core(CoreDumpOptions),
}

/// A request's failure, as the page receives it.
pub struct Failure(ErrorBody);

impl Failure {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self(ErrorBody {
            kind,
            message: message.into(),
        })
    }

    pub fn body(self) -> ErrorBody {
        self.0
    }
}

impl From<uscope::Error> for Failure {
    fn from(error: uscope::Error) -> Self {
        use uscope::Error;
        let kind = match error {
            Error::StaleStop => ErrorKind::StaleStop,
            Error::NotStopped
            | Error::NotRunning
            | Error::AlreadyRunning
            | Error::AlreadyStopped => ErrorKind::NotStopped,
            Error::PostMortemTarget => ErrorKind::Unsupported,
            _ => ErrorKind::Failed,
        };
        Self::new(kind, error.to_string())
    }
}

type Answer = Result<Value, Failure>;

/// The session every connection shares.
pub struct Session {
    cwd: PathBuf,
    home: Option<PathBuf>,
    /// The address links name, such as `127.0.0.1:7341`.
    address: String,
    tokens: Tokens,
    default_name: String,
    /// The current debugger; locked across a change of what is debugged, so
    /// changes happen one at a time.
    target: tokio::sync::Mutex<Option<Target>>,
    state: Arc<watch::Sender<Arc<State>>>,
    /// Output, presence, and notices, already serialized.
    messages: broadcast::Sender<Arc<str>>,
    /// Recent output; its lock also orders publishing output, so a joining
    /// tab sees each piece exactly once.
    history: Arc<Mutex<History>>,
    people: Mutex<BTreeMap<u32, Person>>,
    next_connection: AtomicU32,
}

struct Target {
    debugger: Option<Debugger>,
    handle: DebuggerHandle,
    launch: Option<LaunchSpec>,
    pump: JoinHandle<()>,
    readers: Vec<JoinHandle<()>>,
    name: String,
}

#[derive(Default)]
struct History {
    pieces: VecDeque<Arc<str>>,
    bytes: usize,
}

/// What a joining connection receives before anything else.
pub struct Joined {
    pub connection: u32,
    pub name: String,
    pub history: Vec<Arc<str>>,
    pub messages: broadcast::Receiver<Arc<str>>,
    pub state: watch::Receiver<Arc<State>>,
}

impl Session {
    pub fn new(cwd: PathBuf, address: String, tokens: Tokens) -> Arc<Self> {
        let (state, _) = watch::channel(Arc::new(State::idle(None)));
        let (messages, _) = broadcast::channel(1024);
        let default_name = std::env::var("USER")
            .ok()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "you".to_owned());
        Arc::new(Self {
            cwd,
            home: std::env::var_os("HOME").map(PathBuf::from),
            address,
            tokens,
            default_name,
            target: tokio::sync::Mutex::new(None),
            state: Arc::new(state),
            messages,
            history: Arc::default(),
            people: Mutex::default(),
            next_connection: AtomicU32::new(1),
        })
    }

    pub const fn tokens(&self) -> &Tokens {
        &self.tokens
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The link that joins this server with `role`.
    pub fn join_link(&self, role: Role, to: &str) -> String {
        let to = if to == "/" || to.is_empty() {
            String::new()
        } else {
            format!("?to={to}")
        };
        format!(
            "http://{}/join{to}#{}",
            self.address,
            self.tokens.token(role)
        )
    }

    /// Adds a connection to the presence list.
    pub fn join(&self, role: Role) -> Joined {
        let connection = self.next_connection.fetch_add(1, Ordering::Relaxed);
        let name = match role {
            Role::Control => self.default_name.clone(),
            Role::View => format!("guest-{connection}"),
        };
        let (history, messages) = {
            let history = self.history.lock().expect("history lock");
            (
                history.pieces.iter().cloned().collect(),
                self.messages.subscribe(),
            )
        };
        let state = self.state.subscribe();
        self.people.lock().expect("people lock").insert(
            connection,
            Person {
                connection,
                name: name.clone(),
                role,
            },
        );
        self.announce_presence();
        Joined {
            connection,
            name,
            history,
            messages,
            state,
        }
    }

    pub fn leave(&self, connection: u32) {
        self.people.lock().expect("people lock").remove(&connection);
        self.announce_presence();
    }

    fn announce_presence(&self) {
        let people = self
            .people
            .lock()
            .expect("people lock")
            .values()
            .cloned()
            .collect();
        self.broadcast(&ServerMessage::Presence(Presence { people }));
    }

    fn broadcast(&self, message: &ServerMessage) {
        let text = serde_json::to_string(message).expect("messages serialize");
        let _ = self.messages.send(text.into());
    }

    fn name_of(&self, connection: u32) -> String {
        self.people
            .lock()
            .expect("people lock")
            .get(&connection)
            .map(|person| person.name.clone())
            .unwrap_or_default()
    }

    fn notice(&self, connection: u32, text: impl Into<String>) {
        self.broadcast(&ServerMessage::Notice(protocol::Notice {
            connection,
            name: self.name_of(connection),
            text: text.into(),
        }));
    }

    /// Serves one request from `connection`.
    pub async fn handle(&self, connection: u32, role: Role, request: Request) -> Answer {
        let controls = !matches!(request, Request::SetName(_) | Request::Share(_));
        if controls && role != Role::Control {
            return Err(Failure::new(
                ErrorKind::Forbidden,
                "this link can only view the session; ask for a control link",
            ));
        }
        let notice = match request {
            Request::SetName(protocol::SetName { name }) => {
                self.rename(connection, &name)?;
                None
            }
            Request::Share(protocol::Share { role: wanted, to }) => {
                return self.share(role, wanted, &to);
            }
            Request::CompletePath(protocol::CompletePath { text }) => {
                let entries = picker::complete(&text, &self.cwd, self.home.as_deref());
                return Ok(to_value(&PathCompletions { entries }));
            }
            Request::Processes => {
                return Ok(to_value(&Processes {
                    processes: picker::processes(Path::new("/proc")),
                    ptrace_scope: picker::ptrace_scope(),
                }));
            }
            Request::Launch(launch) => {
                let (replace, run) = (launch.replace, launch.run);
                let spec = self.launch_spec(launch);
                let name = spec.program.display().to_string();
                self.start(Start::Launch { spec, run }, replace).await?;
                Some(format!("launched {name}"))
            }
            Request::Attach(protocol::Attach { pid, replace }) => {
                self.start(Start::Attach(ProcessId::new(pid)), replace)
                    .await?;
                Some(format!("attached to process {pid}"))
            }
            Request::OpenCore(protocol::OpenCore {
                core,
                executable,
                replace,
            }) => {
                let resolve = |text: &str| picker::resolve(text, &self.cwd, self.home.as_deref());
                let mut options = CoreDumpOptions::new(resolve(&core));
                options.executable = executable
                    .filter(|path| !path.is_empty())
                    .map(|path| resolve(&path));
                self.start(Start::Core(options), replace).await?;
                Some(format!("opened {core}"))
            }
            Request::End => Some(self.end_current().await?),
            Request::Continue(protocol::Continue { stop }) => Some(self.resume(stop).await?),
            Request::Pause => {
                self.current_handle().await?.pause().await?;
                Some("paused".to_owned())
            }
            Request::Kill => {
                self.current_handle().await?.kill().await?;
                Some("killed the program".to_owned())
            }
            Request::Restart => {
                self.restart().await?;
                Some("restarted the program".to_owned())
            }
        };
        if let Some(text) = notice {
            self.notice(connection, text);
        }
        Ok(Value::Null)
    }

    fn rename(&self, connection: u32, name: &str) -> Result<(), Failure> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 40 {
            return Err(Failure::new(
                ErrorKind::Invalid,
                "a name has 1 to 40 characters",
            ));
        }
        if let Some(person) = self
            .people
            .lock()
            .expect("people lock")
            .get_mut(&connection)
        {
            name.clone_into(&mut person.name);
        }
        self.announce_presence();
        Ok(())
    }

    fn share(&self, role: Role, wanted: Role, to: &str) -> Answer {
        if wanted == Role::Control && role != Role::Control {
            return Err(Failure::new(
                ErrorKind::Forbidden,
                "only control access can share control",
            ));
        }
        if !to.starts_with('/') {
            return Err(Failure::new(ErrorKind::Invalid, "a link opens a page path"));
        }
        Ok(to_value(&ShareLink {
            url: self.join_link(wanted, to),
        }))
    }

    fn launch_spec(&self, launch: protocol::Launch) -> LaunchSpec {
        let resolve = |text: &str| picker::resolve(text, &self.cwd, self.home.as_deref());
        LaunchSpec {
            program: resolve(&launch.program),
            arguments: launch.arguments.into_iter().map(OsString::from).collect(),
            environment: launch
                .environment
                .into_iter()
                .map(|(name, value)| (name.into(), value.into()))
                .collect(),
            cwd: launch
                .cwd
                .filter(|cwd| !cwd.is_empty())
                .map(|cwd| resolve(&cwd)),
            stop_at_entry: launch.stop_at_entry,
        }
    }

    async fn end_current(&self) -> Result<String, Failure> {
        let current = self.target.lock().await.take();
        let Some(current) = current else {
            return Err(Failure::new(
                ErrorKind::Invalid,
                "nothing is being debugged",
            ));
        };
        let name = current.name.clone();
        let ended = self.end(current).await;
        self.state.send_replace(Arc::new(State::idle(None)));
        ended?;
        Ok(format!("ended the {name} session"))
    }

    /// Continues the stop named, or starts a launched program that is not
    /// running.
    async fn resume(&self, stop: Option<u64>) -> Result<String, Failure> {
        let (handle, launch, started) = self.current_launch().await?;
        if let Some(stop) = stop {
            let InferiorState::Stopped { process_id, .. } = handle.snapshot().await?.inferior
            else {
                return Err(Failure::new(
                    ErrorKind::NotStopped,
                    "the program is not stopped",
                ));
            };
            handle
                .continue_execution(
                    StopId::new(stop),
                    ResumeScope::Process(process_id),
                    ExceptionDisposition::Pass,
                )
                .await?;
            return Ok("continued".to_owned());
        }
        let Some(spec) = launch else {
            return Err(Failure::new(
                ErrorKind::Invalid,
                "name the stop to continue",
            ));
        };
        if started {
            return Err(Failure::new(
                ErrorKind::NotStopped,
                "the program has already started; name the stop to continue",
            ));
        }
        self.run(&handle, &spec).await?;
        Ok("started the program".to_owned())
    }

    async fn restart(&self) -> Result<(), Failure> {
        let (handle, launch, _) = self.current_launch().await?;
        let Some(spec) = launch else {
            return Err(Failure::new(
                ErrorKind::Unsupported,
                "only a launched program can restart",
            ));
        };
        match handle.kill().await {
            Ok(()) | Err(uscope::Error::NotRunning) => {}
            Err(error) => return Err(error.into()),
        }
        self.run(&handle, &spec).await
    }

    async fn current_handle(&self) -> Result<DebuggerHandle, Failure> {
        self.target
            .lock()
            .await
            .as_ref()
            .map(|target| target.handle.clone())
            .ok_or_else(|| Failure::new(ErrorKind::Invalid, "nothing is being debugged"))
    }

    /// The handle, the launch spec of a launched program, and whether the
    /// program is running or stopped, rather than not started or ended.
    async fn current_launch(&self) -> Result<(DebuggerHandle, Option<LaunchSpec>, bool), Failure> {
        let current = self
            .target
            .lock()
            .await
            .as_ref()
            .map(|target| (target.handle.clone(), target.launch.clone()));
        let (handle, launch) =
            current.ok_or_else(|| Failure::new(ErrorKind::Invalid, "nothing is being debugged"))?;
        let started = matches!(
            self.state.borrow().inferior,
            Inferior::Running { .. } | Inferior::Stopped { .. }
        );
        Ok((handle, launch, started))
    }

    /// Makes `start` the session's target, ending a current one when
    /// `replace` allows.
    pub async fn start(&self, start: Start, replace: bool) -> Result<(), Failure> {
        let mut target = self.target.lock().await;
        if let Some(current) = target.as_ref()
            && !replace
        {
            return Err(Failure::new(
                ErrorKind::Busy,
                format!("{} is being debugged; replace it to continue", current.name),
            ));
        }
        let busy = match &start {
            Start::Launch { spec, .. } => format!("Loading {}", spec.program.display()),
            Start::Attach(process) => format!("Attaching to process {process}"),
            Start::Core(options) => format!("Opening {}", options.core.display()),
        };
        if let Some(current) = target.take() {
            let _ = self.end(current).await;
        }
        self.history.lock().expect("history lock").clear();
        self.state.send_replace(Arc::new(State::idle(Some(busy))));
        let opened = open(&start).await;
        let debugger = match opened {
            Ok(debugger) => debugger,
            Err(message) => {
                self.state.send_replace(Arc::new(State::idle(None)));
                return Err(Failure::new(ErrorKind::Failed, message));
            }
        };
        let handle = debugger.handle();
        let (info, launch, run) = match start {
            Start::Launch { spec, run } => (
                protocol::Target {
                    kind: TargetKind::Launch,
                    program: handle.executable().display().to_string(),
                    arguments: spec
                        .arguments
                        .iter()
                        .map(|argument| argument.to_string_lossy().into_owned())
                        .collect(),
                    pid: None,
                },
                Some(spec),
                run,
            ),
            Start::Attach(process) => (
                protocol::Target {
                    kind: TargetKind::Attach,
                    program: handle.executable().display().to_string(),
                    arguments: Vec::new(),
                    pid: Some(process.get()),
                },
                None,
                false,
            ),
            Start::Core(_) => (
                protocol::Target {
                    kind: TargetKind::Core,
                    program: handle.executable().display().to_string(),
                    arguments: Vec::new(),
                    pid: handle.core_dump().map(|core| core.process_id.get()),
                },
                None,
                false,
            ),
        };
        let id =
            random_hex(4).map_err(|error| Failure::new(ErrorKind::Failed, error.to_string()))?;
        let name = Path::new(&info.program).file_name().map_or_else(
            || info.program.clone(),
            |name| name.to_string_lossy().into_owned(),
        );
        let pump = tokio::spawn(pump(
            Arc::clone(&self.state),
            id,
            info,
            handle.clone(),
            handle.subscribe(),
        ));
        *target = Some(Target {
            debugger: Some(debugger),
            handle: handle.clone(),
            launch: launch.clone(),
            pump,
            readers: Vec::new(),
            name,
        });
        if run && let Some(spec) = launch {
            let readers = self.launch(&handle, &spec).await?;
            if let Some(target) = target.as_mut() {
                target.readers = readers;
            }
        }
        Ok(())
    }

    /// Starts a launched program, recording its output readers.
    async fn run(&self, handle: &DebuggerHandle, spec: &LaunchSpec) -> Result<(), Failure> {
        let readers = self.launch(handle, spec).await?;
        if let Some(target) = self.target.lock().await.as_mut() {
            target.readers.extend(readers);
        }
        Ok(())
    }

    async fn launch(
        &self,
        handle: &DebuggerHandle,
        spec: &LaunchSpec,
    ) -> Result<Vec<JoinHandle<()>>, Failure> {
        let failed = |error: std::io::Error| Failure::new(ErrorKind::Failed, error.to_string());
        let (stdout_read, stdout_write) = output::pipe().map_err(failed)?;
        let (stderr_read, stderr_write) = output::pipe().map_err(failed)?;
        let options = LaunchOptions {
            arguments: spec.arguments.clone(),
            environment: spec
                .environment
                .iter()
                .map(|(name, value)| (name.clone(), Some(value.clone())))
                .collect(),
            working_directory: spec.cwd.clone(),
            stdin: Some(std::process::Stdio::null()),
            stdout: Some(stdout_write.into()),
            stderr: Some(stderr_write.into()),
            stop_at_entry: spec.stop_at_entry,
        };
        handle.launch_with(options).await.map_err(|error| {
            Failure::new(
                ErrorKind::Failed,
                format!("failed to launch {}: {error}", spec.program.display()),
            )
        })?;
        Ok(
            [(stdout_read, Stream::Stdout), (stderr_read, Stream::Stderr)]
                .into_iter()
                .filter_map(|(read, stream)| self.read_output(read, stream).ok())
                .collect(),
        )
    }

    /// Forwards a program stream to every tab until its writers close it.
    fn read_output(&self, read: OwnedFd, stream: Stream) -> std::io::Result<JoinHandle<()>> {
        let mut receiver = tokio::net::unix::pipe::Receiver::from_owned_fd(read)?;
        let history = Arc::clone(&self.history);
        let messages = self.messages.clone();
        Ok(tokio::spawn(async move {
            let mut pending = Vec::with_capacity(OUTPUT_CHUNK);
            let mut buffer = vec![0; OUTPUT_CHUNK];
            loop {
                let read = receiver.read(&mut buffer).await.unwrap_or(0);
                pending.extend_from_slice(&buffer[..read]);
                let text = output::decode(&mut pending, read == 0);
                for piece in output::pieces(&text, OUTPUT_CHUNK) {
                    let message = ServerMessage::Output(Output {
                        stream,
                        text: piece.to_owned(),
                    });
                    let text: Arc<str> = serde_json::to_string(&message)
                        .expect("messages serialize")
                        .into();
                    // Publishing under the lock keeps joining tabs from
                    // missing or repeating a piece.
                    let mut history = history.lock().expect("history lock");
                    history.push(Arc::clone(&text));
                    let _ = messages.send(text);
                    drop(history);
                }
                if read == 0 {
                    return;
                }
            }
        }))
    }

    /// Ends a target: a launched program is killed, an attached one
    /// detached.
    async fn end(&self, mut target: Target) -> Result<(), Failure> {
        target.pump.abort();
        let _ = (&mut target.pump).await;
        let shutdown = match target.debugger.take() {
            Some(debugger) => debugger.shutdown().await,
            None => Ok(()),
        };
        // The program's end closes its streams; a reader still waiting is
        // on a stream a forked child kept open.
        for reader in &target.readers {
            reader.abort();
        }
        shutdown.map_err(Failure::from)
    }

    /// Ends whatever is being debugged, as the server exits.
    pub async fn shutdown(&self) {
        let current = self.target.lock().await.take();
        if let Some(target) = current {
            let _ = self.end(target).await;
        }
    }
}

impl History {
    fn push(&mut self, piece: Arc<str>) {
        self.bytes += piece.len();
        self.pieces.push_back(piece);
        while self.bytes > OUTPUT_HISTORY
            && let Some(oldest) = self.pieces.pop_front()
        {
            self.bytes -= oldest.len();
        }
    }

    fn clear(&mut self) {
        self.pieces.clear();
        self.bytes = 0;
    }
}

fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or_else(|error| json!({ "error": error.to_string() }))
}

/// Opens the debugger `start` describes, off the async threads where it
/// loads debug information.
async fn open(start: &Start) -> Result<Debugger, String> {
    match start {
        Start::Launch { spec, .. } => {
            let program = spec.program.clone();
            tokio::task::spawn_blocking(move || Debugger::new(&program))
                .await
                .map_err(|error| error.to_string())?
                .map_err(|error| format!("failed to load {}: {error}", spec.program.display()))
        }
        Start::Attach(process) => Debugger::attach(*process)
            .await
            .map_err(|error| format!("failed to attach to process {process}: {error}")),
        Start::Core(options) => {
            let options = options.clone();
            let core = options.core.display().to_string();
            tokio::task::spawn_blocking(move || Debugger::open_core(&options))
                .await
                .map_err(|error| error.to_string())?
                .map_err(|error| format!("failed to open the core dump {core}: {error}"))
        }
    }
}

/// Publishes the debugger's state after every burst of its events.
async fn pump(
    state: Arc<watch::Sender<Arc<State>>>,
    id: String,
    target: protocol::Target,
    handle: DebuggerHandle,
    mut events: broadcast::Receiver<DebuggerEvent>,
) {
    let mut ended = Ended::default();
    loop {
        let Ok(snapshot) = handle.snapshot().await else {
            return;
        };
        state.send_replace(Arc::new(describe(&id, &target, &snapshot, &ended)));
        match events.recv().await {
            Ok(event) => ended.observe(&event),
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
        // Take every event already queued, so the burst publishes once.
        loop {
            match events.try_recv() {
                Ok(event) => ended.observe(&event),
                Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
    }
}

/// How the program last ended, which snapshots no longer say.
#[derive(Default)]
struct Ended {
    exited: Option<String>,
    detached: Option<u64>,
}

impl Ended {
    fn observe(&mut self, event: &DebuggerEvent) {
        match event {
            DebuggerEvent::InferiorExited { status, .. } => {
                self.exited = Some(format::stop(
                    &StopReason::Exited(status.clone()),
                    Renderer::new(false),
                ));
            }
            DebuggerEvent::InferiorDetached { process_id, .. } => {
                self.detached = Some(process_id.get());
            }
            DebuggerEvent::InferiorLaunched { .. } | DebuggerEvent::InferiorAttached { .. } => {
                *self = Self::default();
            }
            _ => {}
        }
    }
}

fn describe(
    id: &str,
    target: &protocol::Target,
    snapshot: &uscope::StateSnapshot,
    ended: &Ended,
) -> State {
    let inferior = match &snapshot.inferior {
        InferiorState::NotRunning => match (ended.detached, &ended.exited) {
            (Some(pid), _) => Inferior::Detached { pid },
            (None, Some(description)) => Inferior::Exited {
                description: description.clone(),
            },
            (None, None) => Inferior::NotStarted,
        },
        InferiorState::Running { process_id, .. } => Inferior::Running {
            pid: process_id.get(),
        },
        InferiorState::Stopped {
            process_id,
            stop_id,
            thread_id,
            reason,
        } => Inferior::Stopped {
            pid: process_id.get(),
            stop: stop_id.get(),
            thread: thread_id.get(),
            reason: protocol::StopReason {
                kind: reason_kind(reason).to_owned(),
                description: format::stop(reason, Renderer::new(false)),
            },
        },
    };
    State {
        session: Some(id.to_owned()),
        target: Some(target.clone()),
        busy: None,
        revision: snapshot.revision,
        inferior,
        threads: snapshot
            .threads
            .iter()
            .map(|thread| Thread {
                id: thread.id.get(),
                name: thread.name.as_deref().map(str::to_owned),
                stopped: matches!(thread.state, uscope::ThreadState::Stopped { .. }),
            })
            .collect(),
    }
}

const fn reason_kind(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::Attach => "attach",
        StopReason::Entry => "entry",
        StopReason::Breakpoint { .. } => "breakpoint",
        StopReason::Watchpoint { .. } => "watchpoint",
        StopReason::WatchpointInvalidated { .. } => "watchpointInvalidated",
        StopReason::WatchpointArmFailed { .. } => "watchpointArmFailed",
        StopReason::Step { .. } => "step",
        StopReason::StepIncomplete { .. } => "stepIncomplete",
        StopReason::Pause => "pause",
        StopReason::Exception(_) => "exception",
        StopReason::Exec { .. } => "exec",
        StopReason::ThreadExited { .. } => "threadExited",
        StopReason::Unclassifiable { .. } => "unclassifiable",
        StopReason::Exited(_) => "exited",
        StopReason::CoreDump { .. } => "coreDump",
    }
}
