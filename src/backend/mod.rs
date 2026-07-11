#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("uscope currently supports only Linux x86-64");

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux;

use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;

use tokio::sync::{broadcast, mpsc};

use crate::Result;
use crate::protocol::{DebuggerEvent, Request};

pub enum ControllerMessage {
    Request(Request),
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    Wait(linux::WaitEvent),
}

pub fn spawn_controller(
    executable: Arc<PathBuf>,
    message_sender: mpsc::Sender<ControllerMessage>,
    messages: mpsc::Receiver<ControllerMessage>,
    events: broadcast::Sender<DebuggerEvent>,
) -> Result<JoinHandle<()>> {
    linux::spawn_controller(executable, message_sender, messages, events)
}
