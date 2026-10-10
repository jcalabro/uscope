//! The messages the web UI and `uscope web` exchange over the WebSocket.
//!
//! Every message is one JSON text frame. A tab sends [`Envelope`]s; the
//! server answers each with a [`ServerMessage::Result`] or
//! [`ServerMessage::Error`] carrying its `id`, and pushes the rest. The
//! TypeScript side, `web/src/protocol.gen.ts`, is generated from these types
//! by a test that fails when the checked-in file is stale.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Changes whenever a message changes incompatibly; the page refuses a
/// server that speaks another version.
pub const VERSION: u32 = 1;

/// What a connection may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum Role {
    /// Run, stop, and choose what to debug.
    Control,
    /// Watch, and move one's own focus.
    View,
}

/// One request from a tab.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Envelope {
    /// Chosen by the tab; the answer carries it back.
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

/// What a tab can ask for.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "method", content = "params", rename_all = "camelCase")]
pub enum Request {
    /// Names this connection for everyone else's presence list.
    SetName(SetName),
    /// A link that joins this session with the given access.
    Share(Share),
    /// Paths that complete a partly typed one, for the picker.
    CompletePath(CompletePath),
    /// This user's processes, for attaching.
    Processes,
    /// Loads a program to launch, ending any current session.
    Launch(Launch),
    /// Attaches to a process, ending any current session.
    Attach(Attach),
    /// Opens a core dump, ending any current session.
    OpenCore(OpenCore),
    /// Ends the current session: a launched program is killed, an attached
    /// one detached.
    End,
    /// Starts a launched program that has not started, or continues a
    /// stopped one.
    Continue(Continue),
    /// Stops a running program.
    Pause,
    /// Kills the program, which can then be started again.
    Kill,
    /// Kills the program and starts it again.
    Restart,
    /// Steps one thread of a stop.
    Step(Step),
    /// The calls of a thread's line at a stop that a step into can go
    /// into.
    StepTargets(ThreadAt),
    /// Moves one thread of a stop, without running it, to resume at a
    /// location in its function.
    Jump(Jump),
    /// Says where this tab is looking, for everyone's presence list.
    SetFocus(SetFocus),
    /// A thread's stack at a stop.
    Backtrace(ThreadAt),
    /// Every source file the debug information names.
    Sources,
    /// One source file's text, and the lines a breakpoint can stop at.
    Source(SourcePath),
    /// Adds a breakpoint.
    AddBreakpoint(AddBreakpoint),
    /// Replaces a breakpoint's condition, hit condition, and log message.
    EditBreakpoint(EditBreakpoint),
    RemoveBreakpoint(BreakpointRef),
    /// Writes to the program's standard input, or closes it.
    Input(Input),
    /// A frame's arguments and locals, and the statics of its source file.
    Scopes(FrameAt),
    /// A window of what a row expands to.
    Children(ChildrenOf),
    /// Evaluates an expression in a frame without changing the program.
    Evaluate(Evaluate),
    /// Assigns a value to what a path names.
    SetValue(SetValue),
    /// Completes a console line.
    Complete(Complete),
    /// Runs a console line: an expression, which may assign, or one of
    /// uscope's commands, in the tab's frame.
    Console(ConsoleLine),
    /// The instructions of the function holding an address, or around it.
    Disassemble(Disassemble),
    /// Bytes of the program's memory at a stop.
    ReadMemory(ReadMemory),
    /// Writes bytes to the program's memory at a stop.
    WriteMemory(WriteMemory),
    /// A frame's registers.
    Registers(FrameAt),
    /// Stops when memory an expression names, or an address range, changes
    /// or is accessed.
    AddWatchpoint(AddWatchpoint),
    /// Replaces a watchpoint's condition and hit condition.
    EditWatchpoint(EditWatchpoint),
    RemoveWatchpoint(WatchpointRef),
    /// What the debugger does with each signal.
    Signals,
    /// Changes what the debugger does with one signal.
    SetSignal(SignalPolicy),
    /// The modules the program has loaded.
    Modules,
    /// Functions whose names hold the query, best matches first.
    Functions(FunctionQuery),
    /// The tasks of the program's runtimes at a stop.
    Tasks(StopAt),
    /// The inputs of a drawing of a value, which the page's renderer draws.
    Draw(Draw),
    /// A renderer's JavaScript, by its digest.
    Renderer(RendererRef),
    /// Every renderer a drawing may name, for drawing a value with any.
    Renderers,
    /// Reads the session's view files, and the renderers beside them,
    /// again.
    ReloadViews,
}

/// A stop, which must be current.
#[derive(Debug, Clone, Copy, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct StopAt {
    pub stop: u64,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SetName {
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Share {
    pub role: Role,
    /// The page path the link opens, such as `/s/k7q2`.
    pub to: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct CompletePath {
    /// The path typed so far, relative to the server's directory or `~`.
    pub text: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Launch {
    pub program: String,
    #[serde(default)]
    pub arguments: Vec<String>,
    /// The program's working directory, instead of the server's.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Variables set in the program's environment, in order.
    #[serde(default)]
    pub environment: Vec<(String, String)>,
    /// Stop at the program's first instruction when it starts.
    #[serde(default)]
    pub stop_at_entry: bool,
    /// Start the program at once instead of waiting for a continue.
    #[serde(default)]
    pub run: bool,
    /// End a current session to make way. Without it, a current session
    /// fails the request with `busy`.
    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Attach {
    pub pid: u64,
    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct OpenCore {
    pub core: String,
    /// The executable that wrote the dump, when the recorded one is wrong.
    #[serde(default)]
    pub executable: Option<String>,
    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Continue {
    /// The stop being continued, which must still be current. Absent to
    /// start a program that has not started.
    #[serde(default)]
    pub stop: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Step {
    /// The stop being stepped from, which must still be current.
    pub stop: u64,
    pub thread: u64,
    /// The task stepped, which runs on the thread.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub task: Option<TaskKey>,
    /// The frame a step out leaves; every other step starts from the
    /// innermost frame.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub frame: u32,
    pub kind: StepKind,
    /// For a step into, the one call of the line to go into, in
    /// hexadecimal, as `stepTargets` lists it; the line's other calls run
    /// to their returns.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub call: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Jump {
    /// The stop the thread is moved at, which must still be current.
    pub stop: u64,
    pub thread: u64,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub task: Option<TaskKey>,
    /// `FILE:LINE`, or another location a breakpoint takes.
    pub location: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum StepKind {
    /// To the next source line, running calls through.
    Over,
    /// To the next source line, entering calls.
    Into,
    /// Until the frame returns.
    Out,
    /// One instruction, entering calls.
    Instruction,
    /// One instruction, running calls through.
    OverInstruction,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SetFocus {
    /// Absent when the tab looks at nothing in particular.
    pub focus: Option<Focus>,
}

/// Where a person is looking: a page address and a few words for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Focus {
    /// The page path and query, such as `/s/k7q2/stop/12/t/41872/f/1`.
    pub url: String,
    /// Such as `frame 1, serve_conn`.
    pub label: String,
}

/// A task of one of the program's runtimes, as the debugger numbers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct TaskKey {
    pub runtime: u32,
    pub number: u64,
}

impl TaskKey {
    pub const fn id(self) -> uscope::TaskId {
        uscope::TaskId {
            runtime: uscope::RuntimeId::new(self.runtime),
            number: self.number,
        }
    }
}

/// What runs code: a task when one is named, else the thread.
pub const fn execution(thread: u64, task: Option<TaskKey>) -> uscope::ExecutionContext {
    match task {
        Some(task) => uscope::ExecutionContext::Task(task.id()),
        None => uscope::ExecutionContext::Thread(uscope::ThreadId::new(thread)),
    }
}

/// A thread at a stop, or a task, when one is named, whose stack is shown
/// instead.
#[derive(Debug, Clone, Copy, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ThreadAt {
    pub stop: u64,
    pub thread: u64,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub task: Option<TaskKey>,
}

/// A frame of a thread or task at a stop, counting from the innermost.
#[derive(Debug, Clone, Copy, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct FrameAt {
    pub stop: u64,
    pub thread: u64,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub task: Option<TaskKey>,
    pub frame: u32,
}

impl ThreadAt {
    pub const fn execution(self) -> uscope::ExecutionContext {
        execution(self.thread, self.task)
    }
}

impl FrameAt {
    pub const fn execution(self) -> uscope::ExecutionContext {
        execution(self.thread, self.task)
    }
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ChildrenOf {
    /// A row's `children.handle`, which belongs to this connection and the
    /// row's stop.
    pub handle: u64,
    pub start: u64,
    pub count: u64,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Evaluate {
    #[serde(flatten)]
    pub at: FrameAt,
    pub expression: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SetValue {
    #[serde(flatten)]
    pub at: FrameAt,
    /// The expression that names what changes, a row's `path`.
    pub path: String,
    /// An expression for the new value.
    pub value: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Complete {
    /// The line up to the cursor.
    pub text: String,
    /// The frame whose names complete; absent when nothing is stopped.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub stop: Option<u64>,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub thread: Option<u64>,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub task: Option<TaskKey>,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub frame: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ConsoleLine {
    pub line: String,
    /// The frame the line runs in; absent when nothing is stopped.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub stop: Option<u64>,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub thread: Option<u64>,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub task: Option<TaskKey>,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub frame: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Disassemble {
    #[serde(flatten)]
    pub at: FrameAt,
    /// The address to show, as hexadecimal; the frame's own code when
    /// absent.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub address: Option<String>,
    /// Intel unless AT&T is asked for.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub syntax: Option<Syntax>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum Syntax {
    Intel,
    Att,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ReadMemory {
    /// The stop the bytes are read at, which must be current.
    pub stop: u64,
    /// As hexadecimal, such as `0x7ffff7a3e010`.
    pub address: String,
    pub count: u64,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct WriteMemory {
    pub stop: u64,
    pub address: String,
    /// The bytes, as pairs of hexadecimal digits.
    pub bytes: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct AddWatchpoint {
    /// An expression in the frame, or `0xADDRESS:BYTES`.
    pub target: String,
    pub access: WatchAccess,
    /// The frame an expression is resolved in; an address range needs none.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub stop: Option<u64>,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub thread: Option<u64>,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub task: Option<TaskKey>,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub frame: Option<u32>,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub condition: Option<String>,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub hit_condition: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct EditWatchpoint {
    pub id: u64,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub condition: Option<String>,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub hit_condition: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct WatchpointRef {
    pub id: u64,
}

/// What stops a watchpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum WatchAccess {
    /// A store that changes the value.
    Change,
    /// Every store, even of the same value.
    Write,
    /// Every load or store.
    ReadWrite,
    Read,
}

/// What the debugger does with one signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SignalPolicy {
    pub signal: u64,
    /// Such as `SIGUSR1`; filled in by the server.
    #[serde(default)]
    pub name: String,
    pub stop: bool,
    pub print: bool,
    pub pass: bool,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct FunctionQuery {
    pub query: String,
    /// How many to return; 50 when absent, and never more than 200.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SourcePath {
    /// A path as the debug information records it.
    pub path: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct AddBreakpoint {
    /// A function, `FILE:LINE`, `FILE:FUNCTION`, or `0xADDRESS`.
    pub location: String,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub condition: Option<String>,
    /// Which hits stop, such as `>=5` or `%10`.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub hit_condition: Option<String>,
    /// A message to log instead of stopping, with expressions in braces.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub log_message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct EditBreakpoint {
    pub id: u64,
    /// Absent to remove.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub condition: Option<String>,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub hit_condition: Option<String>,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub log_message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct BreakpointRef {
    pub id: u64,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Input {
    pub text: String,
    /// Close the input after the text, so the program reads its end.
    #[serde(default)]
    pub eof: bool,
}

/// Everything the server sends.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServerMessage {
    /// The first message on every connection.
    Hello(Hello),
    /// The whole debugger state, sent on connecting and at every change.
    State(Arc<State>),
    /// A request succeeded.
    Result {
        id: u64,
        #[cfg_attr(test, ts(type = "unknown"))]
        result: Value,
    },
    /// A request failed.
    Error { id: u64, error: ErrorBody },
    /// Bytes the program wrote.
    Output(Output),
    /// Who is connected.
    Presence(Presence),
    /// Something another participant did.
    Notice(Notice),
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Hello {
    pub version: u32,
    /// This connection's number, unique while the server runs.
    pub connection: u32,
    pub role: Role,
    /// The name others see until the tab chooses one.
    pub name: String,
    /// The server's working directory, which relative paths start from.
    pub cwd: String,
}

/// The debugger's state at one revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct State {
    /// Identifies what is being debugged; a new program, process, or core
    /// dump gets a new one. Absent when nothing is.
    pub session: Option<String>,
    /// What is being debugged.
    pub target: Option<Target>,
    /// Slow work under way, such as loading debug information.
    pub busy: Option<String>,
    /// The debugger revision this state reflects.
    pub revision: u64,
    pub inferior: Inferior,
    pub threads: Vec<Thread>,
    pub breakpoints: Vec<Breakpoint>,
    /// The latest stops of this session, oldest first.
    pub stops: Vec<StopEntry>,
    /// Counts changes made to the program's values, which make values read
    /// earlier at the same stop out of date.
    pub writes: u64,
    pub watchpoints: Vec<Watchpoint>,
    /// Counts changes to settings that publish nothing else, such as
    /// signal policies.
    pub settings: u64,
}

impl State {
    /// The state with nothing being debugged.
    pub const fn idle(busy: Option<String>) -> Self {
        Self {
            session: None,
            target: None,
            busy,
            revision: 0,
            inferior: Inferior::NotStarted,
            threads: Vec::new(),
            breakpoints: Vec::new(),
            stops: Vec::new(),
            writes: 0,
            watchpoints: Vec::new(),
            settings: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Target {
    pub kind: TargetKind,
    /// The executable's path.
    pub program: String,
    /// A launched program's arguments.
    pub arguments: Vec<String>,
    /// An attached process, or the process a core dump recorded.
    pub pid: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum TargetKind {
    Launch,
    Attach,
    Core,
}

/// The program's execution state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum Inferior {
    /// A launched program that has not started, or nothing at all.
    NotStarted,
    Running {
        pid: u64,
    },
    Stopped {
        pid: u64,
        /// The stop's number, which requests about it carry.
        stop: u64,
        /// The thread whose event caused the stop.
        thread: u64,
        reason: StopReason,
        /// Where that thread stopped.
        place: Option<Place>,
    },
    /// The program ended; it can be started again.
    Exited {
        description: String,
    },
    /// The debugger let an attached process go.
    Detached {
        pid: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct StopReason {
    /// A short machine-readable kind, such as `breakpoint` or `pause`.
    pub kind: String,
    /// The reason as the CLI says it.
    pub description: String,
}

/// A place in the program's code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Place {
    /// The instruction's address, in hexadecimal.
    pub address: String,
    pub function: Option<String>,
    /// The source file, as the debug information records it.
    pub path: Option<String>,
    pub line: Option<u64>,
}

/// One stop in the session's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct StopEntry {
    pub stop: u64,
    pub thread: u64,
    pub reason: StopReason,
    pub place: Option<Place>,
    /// Who ran the program to it, when someone did.
    pub by: Option<String>,
    /// What they did, such as `stepped over`.
    pub action: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Breakpoint {
    pub id: u64,
    /// What it was set at, as it was asked for.
    pub location: String,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub log_message: Option<String>,
    /// Hits in the current process, including those that did not stop.
    pub hits: u64,
    /// Where it resolved; none while no loaded code has it.
    pub places: Vec<Place>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Thread {
    pub id: u64,
    pub name: Option<String>,
    pub stopped: bool,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ErrorBody {
    pub kind: ErrorKind,
    pub message: String,
}

/// Failures the page handles by kind; the message says the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum ErrorKind {
    /// The stop a request named has passed.
    StaleStop,
    /// The request needs a stopped program.
    NotStopped,
    /// This connection's role does not allow the request.
    Forbidden,
    /// Something is already being debugged, and the request did not ask to
    /// replace it.
    Busy,
    /// The request was malformed.
    Invalid,
    /// The request cannot work for this session, such as running a core
    /// dump.
    Unsupported,
    /// The debugger tried and failed.
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Output {
    pub stream: Stream,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum Stream {
    Stdout,
    Stderr,
    /// Messages logpoints wrote.
    Log,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Presence {
    pub people: Vec<Person>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Person {
    pub connection: u32,
    pub name: String,
    pub role: Role,
    pub focus: Option<Focus>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Notice {
    /// Who did it.
    pub connection: u32,
    pub name: String,
    /// What they did, such as `continued`.
    pub text: String,
}

/// The answer to `completePath`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct PathCompletions {
    pub entries: Vec<PathEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct PathEntry {
    /// The completed text, ending in `/` for a directory.
    pub text: String,
    pub kind: PathKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum PathKind {
    Directory,
    Executable,
    File,
}

/// The answer to `processes`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Processes {
    pub processes: Vec<Process>,
    /// Yama's `ptrace_scope`, when the kernel has it: at 1 or more,
    /// attaching to a process that is not a child may be refused.
    pub ptrace_scope: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Process {
    pub pid: u64,
    /// The command line, or the process's name when it has none.
    pub command: String,
}

/// The answer to `backtrace`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Backtrace {
    pub frames: Vec<Frame>,
    /// Why the stack ends early, when the unwinder could not finish it.
    pub incomplete: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Frame {
    /// Its number, counting from the innermost frame.
    pub index: u32,
    /// The function, symbol, or address.
    pub name: String,
    pub kind: FrameKind,
    /// The instruction or return address, in hexadecimal; for a suspended
    /// task's frame, where it resumes, if that is known.
    pub address: Option<String>,
    /// The file name of the module the code is in.
    pub module: Option<String>,
    pub source: Option<SourceLine>,
    /// For a frame that drives a future whose awaits the stack does not
    /// show in full, why.
    pub unfollowed: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum FrameKind {
    Physical,
    /// A call the compiler inlined into the frame below it.
    Inline,
    /// A signal handler's trampoline.
    Signal,
    /// A function that left by a tail call to the frame below it.
    TailCall,
    /// An async function of a suspended task, which its future holds.
    Async,
    /// The future a suspended task's innermost async function awaits.
    Awaited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SourceLine {
    pub path: String,
    pub line: u64,
    pub column: Option<u64>,
}

/// The answer to `sources`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SourceFiles {
    /// Paths as the debug information records them, sorted.
    pub files: Vec<String>,
    /// Where the program's main function is declared, when it has one.
    pub entry: Option<SourceLine>,
}

/// The answer to `functions`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Functions {
    pub functions: Vec<FunctionMatch>,
    /// Whether more functions match than were returned.
    pub more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct FunctionMatch {
    pub name: String,
    /// Where it is declared, when the debug information says.
    pub path: Option<String>,
    pub line: Option<u64>,
}

/// The answer to `source`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SourceText {
    pub path: String,
    /// The file read, which a source map may have moved.
    pub read: String,
    pub text: String,
    /// The lines a breakpoint can stop at, in order.
    pub breakable: Vec<u64>,
}

/// The answer to `addBreakpoint` and `addWatchpoint`: what was added.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Added {
    pub id: u64,
}

/// The answer to `scopes`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Scopes {
    pub scopes: Vec<Scope>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Scope {
    pub key: ScopeKey,
    pub name: String,
    pub rows: Vec<Row>,
    /// Why the scope's rows could not be read, when they could not.
    pub problem: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum ScopeKey {
    Args,
    Locals,
    Statics,
}

/// One value, as the value tree, watches, and the console show it.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub name: String,
    /// The value's summary, or why there is none.
    pub text: String,
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    /// An expression that evaluates the value again.
    pub path: Option<String>,
    pub children: Option<Children>,
    /// Whether `setValue` can change it.
    pub editable: bool,
    /// The address of its natural memory, as hexadecimal.
    pub memory: Option<String>,
    /// How many bytes the value occupies there, when it is stored there.
    pub memory_bytes: Option<u64>,
    /// A row that only says where reading stopped short.
    pub truncated: bool,
    /// The renderers of the drawings the value's view offers, each once.
    pub drawings: Vec<String>,
}

/// What a row expands to.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Children {
    pub handle: u64,
    /// How many children are elements, when it is known.
    pub indexed: Option<u64>,
    /// How many are named, when it is known.
    pub named: Option<u64>,
}

/// The answer to `children`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Rows {
    pub rows: Vec<Row>,
}

/// The answer to `complete`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Completions {
    /// Where the completed part starts, in characters.
    pub start: u64,
    pub items: Vec<Completion>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Completion {
    pub label: String,
    /// `keyword`, `value`, `variable`, or `field`.
    pub kind: String,
}

/// The answer to `console`: a command's output, or an expression's value.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ConsoleResult {
    pub output: Option<String>,
    pub row: Option<Row>,
}

/// A watchpoint, as every tab shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Watchpoint {
    pub id: u64,
    pub access: WatchAccess,
    /// The expression it watches, when it was given one.
    pub expression: Option<String>,
    pub address: String,
    pub bytes: u64,
    /// Such as `thread 41872's frame at 0x7ffe…, until it returns` for a
    /// local, which ends with its frame; empty for a global.
    pub scope: String,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub hits: u64,
}

/// The answer to `disassemble`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Disassembled {
    /// The function shown, when the code shown is one.
    pub function: Option<String>,
    /// The frame's instruction: its program counter, or, in a caller, the
    /// call it returns to after.
    pub marked: Option<String>,
    pub instructions: Vec<Instruction>,
    /// What the code shown leaves out, such as unreadable memory.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Instruction {
    pub address: String,
    /// Its bytes, as hexadecimal pairs separated by spaces.
    pub bytes: String,
    /// Its text in pieces; empty when the bytes decode to nothing.
    pub tokens: Vec<Token>,
    /// Why there is no instruction, when there is none.
    pub invalid: Option<String>,
    /// Where a branch goes, which a page can follow.
    pub target: Option<BranchTarget>,
    /// What its operands name, such as a function or a global.
    pub comment: Option<String>,
    /// The symbol it is in, with its offset, such as `main+12`.
    pub symbol: Option<String>,
    /// Its source line, when it starts one.
    pub source: Option<SourceLine>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Token {
    /// `mnemonic`, `prefix`, `keyword`, `register`, `number`, `address`,
    /// `punctuation`, or `text`.
    pub kind: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct BranchTarget {
    pub address: String,
    pub name: Option<String>,
}

/// A drawing of a value at a frame.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Draw {
    #[serde(flatten)]
    pub at: FrameAt,
    /// The value's `path`, which evaluates it.
    pub path: String,
    /// The renderer: one whose drawing the value's view offers, or else any
    /// renderer, which then draws the value itself as its `values`.
    pub renderer: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RendererRef {
    pub digest: String,
}

/// A renderer, by name, where it was loaded from, and the digest of its
/// JavaScript.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RendererInfo {
    pub name: String,
    /// The file or module record it was loaded from, or `built-in`.
    pub origin: String,
    pub digest: String,
}

/// The answer to `renderer`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RendererSource {
    #[serde(flatten)]
    pub info: RendererInfo,
    pub source: String,
}

/// The answer to `renderers`: each name once, the one a drawing would use.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RendererList {
    pub renderers: Vec<RendererInfo>,
}

/// The answer to `draw`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Drawing {
    pub renderer: RendererInfo,
    /// Whether the value's view offers the drawing; otherwise the renderer
    /// draws the value itself as its `values`.
    pub offered: bool,
    /// Each input, by name, in order; none when a problem kept any part of
    /// them from being read.
    pub inputs: Option<Vec<DrawInput>>,
    /// Why the inputs could not be read whole: the first part that could
    /// not, and why.
    pub problem: Option<String>,
    /// How many bytes the binary frame sent just before this answer holds:
    /// the bytes and numbers the inputs' `bytes` and `numbers` lie in.
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct DrawInput {
    pub name: String,
    pub value: Datum,
}

/// A value as a renderer receives it, decided by its type alone.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum Datum {
    Bool {
        b: bool,
    },
    /// An integer of at most 32 bits, or one the view writes, such as
    /// `64`, that a double holds exactly.
    Int {
        i: i64,
    },
    /// An integer of 64 or 128 bits, a pointer, or an address, in decimal.
    Big {
        big: String,
    },
    /// A float, as the shortest text that reads back as its nearest
    /// double: `NaN`, `Infinity`, and `-Infinity` included.
    Float {
        f: String,
    },
    Text {
        s: String,
    },
    /// An enumeration's value and the enumerator it equals.
    Enum {
        name: Option<String>,
        value: Box<Self>,
    },
    /// The variant a sum type holds, and its payload.
    Sum {
        variant: String,
        value: Option<Box<Self>>,
    },
    Record {
        members: Vec<(String, Self)>,
    },
    List {
        items: Vec<Self>,
    },
    /// Numbers of one kind, `count` of them at `offset` in the drawing's
    /// bytes, little-endian.
    Numbers {
        kind: NumberKind,
        offset: u64,
        count: u64,
    },
    /// `length` bytes at `offset` in the drawing's bytes.
    Bytes {
        offset: u64,
        length: u64,
    },
    /// A map's entries, each a key and a value, in its view's order.
    Entries {
        entries: Vec<(Self, Self)>,
    },
    Null,
}

/// The kind of number each of a sequence's elements is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum NumberKind {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
}

/// The answer to `readMemory`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Memory {
    pub address: String,
    /// The bytes read, as hexadecimal pairs with no separator.
    pub bytes: String,
    /// The first address that could not be read, when the read stopped
    /// short.
    pub unreadable: Option<String>,
}

/// The calls a step into can go into, in address order.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct StepTargets {
    pub calls: Vec<StepCall>,
}

/// One call of a line.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct StepCall {
    /// The call instruction's address, in hexadecimal, which names it to a
    /// step.
    pub call: String,
    /// The function it calls, when something names it.
    pub callee: Option<String>,
    /// The address a direct call calls, in hexadecimal; none for an
    /// indirect call.
    pub target: Option<String>,
}

/// The tasks of the program's runtimes at a stop.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct TaskList {
    /// What the runtimes call a task, such as `task` or `goroutine`.
    pub noun: Option<String>,
    pub tasks: Vec<Task>,
    /// Whether there are more tasks than those listed, which the list
    /// leaves out to stay small.
    pub more: bool,
    /// Why the list may be missing tasks, or describe some wrongly.
    pub gaps: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Task {
    pub key: TaskKey,
    pub state: TaskState,
    /// The code the program wrote that it is in, as the CLI says it.
    pub place: String,
    /// The frame of that code in the task's stack, which choosing the task
    /// shows; absent for a task that runs only its runtime's code.
    pub frame: Option<Frame>,
    /// The runtime's own words for what it does or waits for.
    pub detail: Option<String>,
    /// Labels the program or runtime gave it, such as `{runtime: "local
    /// set 3"}`.
    pub labels: Option<String>,
    /// The thread running it.
    pub thread: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum TaskState {
    Running,
    Runnable,
    Blocked,
    Exited,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Registers {
    pub registers: Vec<Register>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Register {
    pub name: String,
    /// As hexadecimal; absent where a caller's frame did not save it.
    pub value: Option<String>,
    pub bits: u16,
    /// `pc`, `sp`, or `fp`.
    pub role: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Signals {
    pub signals: Vec<SignalPolicy>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Modules {
    pub modules: Vec<Module>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Module {
    pub id: u64,
    pub name: String,
    pub path: String,
    /// Where it is loaded, as hexadecimal.
    pub start: Option<String>,
    pub end: Option<String>,
    /// `debug` with debug information, `symbols` with only a symbol table,
    /// or `none`.
    pub symbols: String,
    /// The separate file its debug information came from, when its own
    /// file was stripped of it.
    pub debug_file: Option<String>,
    /// Why the separate debug file found for it could not be used.
    pub debug_file_problem: Option<String>,
    /// Why not all of its debug information could be read.
    pub debug_information_problem: Option<String>,
}

/// The answer to `share`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ShareLink {
    pub url: String,
}

#[cfg(test)]
mod tests {
    use ts_rs::{Config, TS};

    use super::*;

    const GENERATED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/web/src/protocol.gen.ts");

    /// Every message type, in the order the TypeScript declares them.
    const DECLARATIONS: &[fn(&Config) -> String] = &[
        Role::decl,
        Envelope::decl,
        Request::decl,
        SetName::decl,
        Share::decl,
        CompletePath::decl,
        Launch::decl,
        Attach::decl,
        OpenCore::decl,
        Continue::decl,
        Step::decl,
        Jump::decl,
        StepKind::decl,
        SetFocus::decl,
        Focus::decl,
        ThreadAt::decl,
        SourcePath::decl,
        AddBreakpoint::decl,
        EditBreakpoint::decl,
        BreakpointRef::decl,
        Input::decl,
        ServerMessage::decl,
        Hello::decl,
        State::decl,
        Target::decl,
        TargetKind::decl,
        Inferior::decl,
        StopReason::decl,
        Place::decl,
        StopEntry::decl,
        Breakpoint::decl,
        Thread::decl,
        ErrorBody::decl,
        ErrorKind::decl,
        Output::decl,
        Stream::decl,
        Presence::decl,
        Person::decl,
        Notice::decl,
        PathCompletions::decl,
        PathEntry::decl,
        PathKind::decl,
        Processes::decl,
        Process::decl,
        ShareLink::decl,
        Backtrace::decl,
        Frame::decl,
        FrameKind::decl,
        SourceLine::decl,
        SourceFiles::decl,
        FunctionQuery::decl,
        Functions::decl,
        FunctionMatch::decl,
        SourceText::decl,
        Added::decl,
        FrameAt::decl,
        ChildrenOf::decl,
        Evaluate::decl,
        SetValue::decl,
        Complete::decl,
        ConsoleLine::decl,
        Scopes::decl,
        Scope::decl,
        ScopeKey::decl,
        Row::decl,
        Children::decl,
        Rows::decl,
        Completions::decl,
        Completion::decl,
        ConsoleResult::decl,
        Disassemble::decl,
        ReadMemory::decl,
        WriteMemory::decl,
        AddWatchpoint::decl,
        EditWatchpoint::decl,
        WatchpointRef::decl,
        WatchAccess::decl,
        Syntax::decl,
        SignalPolicy::decl,
        Watchpoint::decl,
        Disassembled::decl,
        Instruction::decl,
        Token::decl,
        BranchTarget::decl,
        Memory::decl,
        StepTargets::decl,
        StepCall::decl,
        Registers::decl,
        Register::decl,
        Signals::decl,
        Modules::decl,
        Module::decl,
        StopAt::decl,
        TaskKey::decl,
        TaskList::decl,
        Task::decl,
        TaskState::decl,
        Draw::decl,
        RendererRef::decl,
        RendererInfo::decl,
        RendererSource::decl,
        RendererList::decl,
        Drawing::decl,
        DrawInput::decl,
        Datum::decl,
        NumberKind::decl,
    ];

    /// Writes every message type as TypeScript.
    fn typescript() -> String {
        let config = Config::new().with_large_int("number");
        let mut text = format!(
            "// Generated from src/web/protocol.rs by `cargo test`; do not edit.\n\n\
             export const PROTOCOL_VERSION = {VERSION};\n"
        );
        for declare in DECLARATIONS {
            text.push('\n');
            text.push_str("export ");
            text.push_str(&declare(&config));
            text.push('\n');
        }
        text
    }

    /// Fails when the checked-in TypeScript is stale. Set
    /// `USCOPE_UPDATE_PROTOCOL=1` to rewrite it.
    #[test]
    fn typescript_matches_the_protocol() {
        let expected = typescript();
        if std::env::var_os("USCOPE_UPDATE_PROTOCOL").is_some() {
            std::fs::write(GENERATED, &expected).expect("write the generated protocol");
            return;
        }
        let actual = std::fs::read_to_string(GENERATED).unwrap_or_default();
        assert!(
            actual == expected,
            "web/src/protocol.gen.ts is stale; rerun with USCOPE_UPDATE_PROTOCOL=1"
        );
    }

    #[test]
    fn requests_parse_with_and_without_parameters() {
        let parse = |text: &str| serde_json::from_str::<Envelope>(text).expect(text);
        assert!(matches!(
            parse(r#"{"id":1,"method":"pause"}"#).request,
            Request::Pause
        ));
        assert!(matches!(
            parse(r#"{"id":2,"method":"continue","params":{"stop":7}}"#).request,
            Request::Continue(Continue { stop: Some(7) })
        ));
        let Request::Launch(launch) = parse(
            r#"{"id":3,"method":"launch","params":{"program":"./a","environment":[["A","1"]]}}"#,
        )
        .request
        else {
            panic!("a launch");
        };
        assert_eq!(launch.environment, [("A".to_owned(), "1".to_owned())]);
        assert!(!launch.replace && !launch.run);
        assert!(
            serde_json::from_str::<Envelope>(r#"{"id":4,"method":"format the disk"}"#).is_err()
        );
    }
}
