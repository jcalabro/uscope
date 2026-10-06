#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("uscope currently supports only Linux x86-64");

mod linux;

use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use tokio::sync::{broadcast, mpsc};

use crate::debug_info::DebugInfo;
use crate::protocol::{CoreDumpOptions, DebuggerEvent, Request};
use crate::{Error, ProcessId, Result};

pub use linux::PostMortemSession;
#[cfg(test)]
pub use linux::native_tracee;
#[cfg(any(test, feature = "sim"))]
pub use linux::sim_edge;

/// Identifies a file independently of the path used to open it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    pub inode: u64,
}

/// An executable's contents and identity, read once when a session starts.
pub struct ExecutableSource {
    pub display_path: Arc<PathBuf>,
    pub data: Arc<[u8]>,
    pub identity: FileIdentity,
    /// For an attach target, the start time that keeps a recycled process ID
    /// from being mistaken for the process whose executable was read.
    pub process_start_time: Option<u64>,
}

pub fn executable_source(path: &Path) -> Result<ExecutableSource> {
    let path = path.canonicalize()?;
    read_executable_source(&path, path.clone(), None)
}

/// Reads a running process's executable through `/proc`, which works even
/// when the file has since been replaced or unlinked.
pub fn process_executable_source(process: ProcessId) -> Result<ExecutableSource> {
    let raw = i32::try_from(process.get())
        .ok()
        .filter(|raw| *raw > 0)
        .ok_or(Error::InvalidProcessId(process.get()))?;
    let (proc_exe, display) = process_exe_link(raw)?;
    let start_time =
        process_start_time(raw).ok_or(Error::ProcessIdentityUnavailable(process.get()))?;
    read_executable_source(&proc_exe, display, Some(start_time))
}

/// Finds a process's `exe` link and the path it names. A leader that exited
/// before the rest of its process has none, so a live thread's is used.
fn process_exe_link(process: i32) -> std::io::Result<(PathBuf, PathBuf)> {
    let leader = PathBuf::from(format!("/proc/{process}/exe"));
    let error = match fs::read_link(&leader) {
        Ok(display) => return Ok((leader, display)),
        Err(error) => error,
    };
    let Ok(mut threads) = fs::read_dir(format!("/proc/{process}/task")) else {
        return Err(error);
    };
    threads
        .find_map(|thread| {
            let link = thread.ok()?.path().join("exe");
            let display = fs::read_link(&link).ok()?;
            Some((link, display))
        })
        .ok_or(error)
}

fn read_executable_source(
    read_path: &Path,
    display_path: PathBuf,
    process_start_time: Option<u64>,
) -> Result<ExecutableSource> {
    let metadata = fs::metadata(read_path)?;
    Ok(ExecutableSource {
        display_path: Arc::new(display_path),
        data: fs::read(read_path)?.into(),
        identity: FileIdentity {
            inode: metadata.ino(),
        },
        process_start_time,
    })
}

/// Returns a process's start time in clock ticks since boot, from field 22
/// of `/proc/<pid>/stat`.
pub fn process_start_time(process: i32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{process}/stat")).ok()?;
    // The command name may contain spaces and parentheses; fields resume
    // after its final closing parenthesis.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

pub enum ControllerMessage {
    Request(Request),
    Wait(linux::WaitEvent),
}

impl ControllerMessage {
    /// Whether the controller serves the message before inspection queued
    /// ahead of it: run control, and the process events it classifies. A
    /// burst of inspection never delays a step.
    pub(crate) const fn preempts_inspection(&self) -> bool {
        matches!(
            self,
            Self::Wait(_)
                | Self::Request(
                    Request::Continue { .. }
                        | Request::Step { .. }
                        | Request::Pause { .. }
                        | Request::Kill { .. }
                        | Request::Terminate { .. }
                        | Request::Shutdown { .. }
                )
        )
    }

    /// Whether the message only reads the stop it names, so it may wait
    /// behind later run control and then fail as a stale request does.
    pub(crate) const fn reads_one_stop(&self) -> bool {
        matches!(
            self,
            Self::Request(
                Request::Variables { .. }
                    | Request::Evaluate {
                        mode: crate::EvaluationMode::Read,
                        ..
                    }
                    | Request::ExpressionType { .. }
                    | Request::ExplainView { .. }
                    | Request::RecordKernels { .. }
                    | Request::Dereference { .. }
                    | Request::ValueChildren { .. }
                    | Request::Backtrace { .. }
                    | Request::Registers { .. }
                    | Request::ReadMemory { .. }
                    | Request::Disassemble { .. }
                    | Request::DescribeAddress { .. }
                    | Request::StoppedLocation { .. }
            )
        )
    }
}

/// The channels a controller serves.
pub struct ControllerChannels {
    /// Lets the controller's waiter thread queue native events.
    pub sender: mpsc::Sender<ControllerMessage>,
    pub receiver: mpsc::Receiver<ControllerMessage>,
    pub events: EventSender,
}

/// Publishes debugger events to every subscribed client, recording each in
/// the flight recorder of a development build.
pub struct EventSender(broadcast::Sender<DebuggerEvent>);

impl EventSender {
    pub fn send(
        &self,
        event: DebuggerEvent,
    ) -> std::result::Result<usize, broadcast::error::SendError<DebuggerEvent>> {
        record!("event {event:?}");
        self.0.send(event)
    }
}

impl From<broadcast::Sender<DebuggerEvent>> for EventSender {
    fn from(sender: broadcast::Sender<DebuggerEvent>) -> Self {
        Self(sender)
    }
}

/// Starts the controller for a live session of `executable`.
pub fn spawn_controller(
    executable: ExecutableSource,
    debug_info: DebugInfo,
    channels: ControllerChannels,
) -> Result<JoinHandle<()>> {
    linux::spawn_controller(executable, debug_info, channels)
}

/// Opens a core dump and starts the controller serving its stopped snapshot.
pub fn open_core(
    options: &CoreDumpOptions,
    channels: ControllerChannels,
) -> Result<PostMortemSession> {
    linux::open_core(options, channels)
}

/// Makes TLS lookups in this process bypass the platform's thread library.
pub fn force_internal_tls_lookup(forced: bool) {
    linux::force_internal_tls_lookup(forced);
}

/// Finds a signal's exception code by name.
pub fn signal_named(name: &str) -> Option<u64> {
    linux::Signal::named(name).map(linux::Signal::code)
}

/// Names the signal with an exception code.
pub fn signal_name(code: u64) -> Option<String> {
    linux::Signal::from_code(code).map(linux::Signal::name)
}

/// The exception codes of every signal, in order.
pub fn signal_codes() -> impl Iterator<Item = u64> {
    linux::Signal::all().map(linux::Signal::code)
}

/// Describes the platform's hardware watchpoint support.
pub fn watchpoint_capabilities() -> crate::WatchpointCapabilities {
    linux::watchpoint_capabilities()
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_core_dump(data: &[u8]) {
    linux::fuzz_core_dump(data);
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_debug_register_plan(data: &[u8]) {
    linux::fuzz_debug_register_plan(data);
}
