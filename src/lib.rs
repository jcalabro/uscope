mod backend;
mod debug_info;
mod error;
mod protocol;

pub use error::{Error, Result};
pub use protocol::{
    BreakpointSpec, DebuggerEvent, ExceptionInfo, ExitStatus, InferiorState, ProcessId,
    StateSnapshot, StopReason,
};

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::timeout;

use backend::ControllerMessage;
use debug_info::DebugInfo;
use protocol::Request;

const REQUEST_CAPACITY: usize = 32;
const EVENT_CAPACITY: usize = 256;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Debugger {
    handle: DebuggerHandle,
    controller: Option<JoinHandle<()>>,
    shutdown_permit: Option<mpsc::OwnedPermit<ControllerMessage>>,
}

#[derive(Clone)]
pub struct DebuggerHandle {
    executable: Arc<PathBuf>,
    debug_info: Arc<dyn DebugInfo>,
    requests: mpsc::Sender<ControllerMessage>,
    events: broadcast::Sender<DebuggerEvent>,
}

impl Debugger {
    pub fn new(executable: impl AsRef<Path>) -> Result<Self> {
        let executable = Arc::new(executable.as_ref().canonicalize()?);
        let debug_info = debug_info::load(&executable)?;
        let (requests, receiver) = mpsc::channel(REQUEST_CAPACITY);
        let shutdown_permit = requests
            .clone()
            .try_reserve_owned()
            .expect("new request channel has shutdown capacity");
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let controller = backend::spawn_controller(
            Arc::clone(&executable),
            requests.clone(),
            receiver,
            events.clone(),
        )?;

        Ok(Self {
            handle: DebuggerHandle {
                executable,
                debug_info,
                requests,
                events,
            },
            controller: Some(controller),
            shutdown_permit: Some(shutdown_permit),
        })
    }

    #[must_use]
    pub fn handle(&self) -> DebuggerHandle {
        self.handle.clone()
    }

    pub async fn shutdown(mut self) -> Result<()> {
        let (send, receive) = oneshot::channel();
        self.shutdown_permit
            .take()
            .expect("shutdown permit is present")
            .send(ControllerMessage::Request(Request::Shutdown {
                reply: send,
            }));
        let result = match timeout(SHUTDOWN_TIMEOUT, receive).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(Error::RequestCancelled),
            Err(_) => return Err(Error::ShutdownTimedOut),
        };

        if let Some(controller) = self.controller.take() {
            tokio::task::spawn_blocking(move || controller.join())
                .await
                .map_err(|_| Error::BackendThreadPanicked)?
                .map_err(|_| Error::BackendThreadPanicked)?;
        }

        result
    }
}

impl Drop for Debugger {
    fn drop(&mut self) {
        if let Some(permit) = self.shutdown_permit.take() {
            let (reply, _) = oneshot::channel();
            permit.send(ControllerMessage::Request(Request::Shutdown { reply }));
        }
    }
}

impl DebuggerHandle {
    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<DebuggerEvent> {
        self.events.subscribe()
    }

    pub async fn add_breakpoint(&self, spec: BreakpointSpec) -> Result<u64> {
        let (address, relocate) = match spec {
            BreakpointSpec::Address(address) => (address, false),
            BreakpointSpec::Function(name) => (self.debug_info.function_address(&name)?, true),
        };

        self.request(|reply| Request::AddBreakpoint {
            address,
            relocate,
            reply,
        })
        .await?;

        Ok(address)
    }

    pub async fn run(&self) -> Result<StopReason> {
        self.request(|reply| Request::Launch { reply }).await
    }

    pub async fn resume(&self) -> Result<StopReason> {
        self.request(|reply| Request::Continue { reply }).await
    }

    pub async fn read_word(&self, address: u64) -> Result<u64> {
        self.request(|reply| Request::ReadWord { address, reply })
            .await
    }

    pub async fn runtime_address(&self, name: &str) -> Result<u64> {
        let link_address = self.debug_info.symbol_address(name)?;

        self.request(|reply| Request::Relocate {
            link_address,
            reply,
        })
        .await
    }

    pub async fn snapshot(&self) -> Result<StateSnapshot> {
        self.request(|reply| Request::Snapshot { reply }).await
    }

    async fn request<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> Request,
    ) -> Result<T> {
        let (send, receive) = oneshot::channel();

        self.requests
            .send(ControllerMessage::Request(make(send)))
            .await
            .map_err(|_| Error::RequestQueueClosed)?;

        receive.await.map_err(|_| Error::RequestCancelled)?
    }
}
