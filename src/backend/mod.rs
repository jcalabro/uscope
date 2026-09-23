#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("uscope currently supports only Linux x86-64");

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux;

use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use tokio::sync::{broadcast, mpsc};

use crate::debug_info::{UnwindInfo, VariableInfo};
use crate::protocol::{DebuggerEvent, Request};
use crate::{ModuleImage, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    pub inode: u64,
}

pub struct ExecutableSource {
    pub display_path: Arc<PathBuf>,
    pub data: Arc<[u8]>,
    pub identity: FileIdentity,
    pub process_start_time: Option<u64>,
}

pub fn executable_source(path: &Path) -> Result<ExecutableSource> {
    let path = path.canonicalize()?;
    read_executable_source(&path, path.clone(), None)
}

pub fn process_executable_source(process: crate::ProcessId) -> Result<ExecutableSource> {
    let raw = i32::try_from(process.get()).map_err(|_| crate::Error::AddressOverflow)?;
    if raw <= 0 {
        return Err(crate::Error::InvalidProcessId(process.get()));
    }
    let proc_exe = PathBuf::from(format!("/proc/{raw}/exe"));
    let display = fs::read_link(&proc_exe)?;
    let stat = fs::read_to_string(format!("/proc/{raw}/stat"))?;
    let start_time =
        process_start_time(&stat).ok_or(crate::Error::ProcessIdentityUnavailable(process.get()))?;
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

fn process_start_time(stat: &str) -> Option<u64> {
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

pub enum ControllerMessage {
    Request(Request),
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    Wait(linux::WaitEvent),
}

pub fn spawn_controller(
    executable: ExecutableSource,
    module_image: Arc<ModuleImage>,
    unwind_info: Arc<dyn UnwindInfo>,
    variable_info: Arc<dyn VariableInfo>,
    message_sender: mpsc::Sender<ControllerMessage>,
    messages: mpsc::Receiver<ControllerMessage>,
    events: broadcast::Sender<DebuggerEvent>,
) -> Result<JoinHandle<()>> {
    linux::spawn_controller(
        executable,
        module_image,
        unwind_info,
        variable_info,
        message_sender,
        messages,
        events,
    )
}
