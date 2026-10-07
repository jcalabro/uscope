//! The messages the web UI and `uscope web` exchange over the WebSocket.
//!
//! Every message is one JSON text frame. A tab sends [`Envelope`]s; the
//! server answers each with a [`ServerMessage::Result`] or
//! [`ServerMessage::Error`] carrying its `id`, and pushes the rest. The
//! TypeScript side, `web/src/protocol.gen.ts`, is generated from these types
//! by a test that fails when the checked-in file is stale.

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

/// Everything the server sends.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServerMessage {
    /// The first message on every connection.
    Hello(Hello),
    /// The whole debugger state, sent on connecting and at every change.
    State(State),
    /// A request succeeded.
    Result { id: u64, result: Value },
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

    /// Writes every message type as TypeScript.
    fn typescript() -> String {
        let config = Config::new().with_large_int("number");
        let declarations = [
            Role::decl(&config),
            Envelope::decl(&config),
            Request::decl(&config),
            SetName::decl(&config),
            Share::decl(&config),
            CompletePath::decl(&config),
            Launch::decl(&config),
            Attach::decl(&config),
            OpenCore::decl(&config),
            Continue::decl(&config),
            ServerMessage::decl(&config),
            Hello::decl(&config),
            State::decl(&config),
            Target::decl(&config),
            TargetKind::decl(&config),
            Inferior::decl(&config),
            StopReason::decl(&config),
            Thread::decl(&config),
            ErrorBody::decl(&config),
            ErrorKind::decl(&config),
            Output::decl(&config),
            Stream::decl(&config),
            Presence::decl(&config),
            Person::decl(&config),
            Notice::decl(&config),
            PathCompletions::decl(&config),
            PathEntry::decl(&config),
            PathKind::decl(&config),
            Processes::decl(&config),
            Process::decl(&config),
            ShareLink::decl(&config),
        ];
        let mut text = format!(
            "// Generated from src/web/protocol.rs by `cargo test`; do not edit.\n\n\
             export const PROTOCOL_VERSION = {VERSION};\n"
        );
        for declaration in declarations {
            text.push('\n');
            text.push_str("export ");
            text.push_str(&declaration);
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
