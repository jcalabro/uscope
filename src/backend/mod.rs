#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("uscope currently supports only Linux x86-64");

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux;

use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;

use tokio::sync::{broadcast, mpsc};

use crate::debug_info::UnwindInfo;
use crate::protocol::{DebuggerEvent, Request};
use crate::{ModuleImage, Result};

pub enum ControllerMessage {
    Request(Request),
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    Wait(linux::WaitEvent),
}

pub fn spawn_controller(
    executable: Arc<PathBuf>,
    module_image: Arc<ModuleImage>,
    unwind_info: Arc<dyn UnwindInfo>,
    message_sender: mpsc::Sender<ControllerMessage>,
    messages: mpsc::Receiver<ControllerMessage>,
    events: broadcast::Sender<DebuggerEvent>,
) -> Result<JoinHandle<()>> {
    linux::spawn_controller(
        executable,
        module_image,
        unwind_info,
        message_sender,
        messages,
        events,
    )
}
