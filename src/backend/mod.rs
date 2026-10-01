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
    let proc_exe = PathBuf::from(format!("/proc/{raw}/exe"));
    let display = fs::read_link(&proc_exe)?;
    let start_time =
        process_start_time(raw).ok_or(Error::ProcessIdentityUnavailable(process.get()))?;
    read_executable_source(&proc_exe, display, Some(start_time))
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

/// The channels a controller serves.
pub struct ControllerChannels {
    /// Lets the controller's waiter thread queue native events.
    pub sender: mpsc::Sender<ControllerMessage>,
    pub receiver: mpsc::Receiver<ControllerMessage>,
    pub events: broadcast::Sender<DebuggerEvent>,
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
