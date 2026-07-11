mod backend;
mod debug_info;
mod error;
pub(crate) mod model;
mod protocol;

pub use error::{Error, Result};
pub use model::{
    AddressRange, Architecture, BreakpointLocation, ByteOrder, ColumnNumber, ExecutionLocation,
    FunctionId, FunctionInfo, ImageAddress, ImageLocation, LineNumber, LoadedModule, ModuleId,
    ModuleImage, ModuleImageId, PointerWidth, SourceFile, SourceFileId, SourceLocation, SymbolId,
    SymbolInfo, TargetDescription, VirtualAddress,
};
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
    module_image: Arc<ModuleImage>,
    requests: mpsc::Sender<ControllerMessage>,
    events: broadcast::Sender<DebuggerEvent>,
}

impl Debugger {
    /// Creates a debugger for a native executable and starts its backend controller.
    pub fn new(executable: impl AsRef<Path>) -> Result<Self> {
        let executable = Arc::new(executable.as_ref().canonicalize()?);
        let module_image = debug_info::load(&executable)?;
        let (requests, receiver) = mpsc::channel(REQUEST_CAPACITY);
        let shutdown_permit = requests
            .clone()
            .try_reserve_owned()
            .expect("new request channel has shutdown capacity");
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let controller = backend::spawn_controller(
            Arc::clone(&executable),
            module_image.id(),
            requests.clone(),
            receiver,
            events.clone(),
        )?;

        Ok(Self {
            handle: DebuggerHandle {
                executable,
                module_image,
                requests,
                events,
            },
            controller: Some(controller),
            shutdown_permit: Some(shutdown_permit),
        })
    }

    #[must_use]
    /// Returns a clonable handle for sending requests to the debugger.
    pub fn handle(&self) -> DebuggerHandle {
        self.handle.clone()
    }

    /// Stops and reaps the inferior, then joins the backend controller.
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
    /// Returns the canonical path of the executable being debugged.
    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Returns the immutable debug metadata for the main executable.
    #[must_use]
    pub const fn module_image(&self) -> &Arc<ModuleImage> {
        &self.module_image
    }

    #[must_use]
    /// Subscribes to debugger state and lifecycle events.
    pub fn subscribe(&self) -> broadcast::Receiver<DebuggerEvent> {
        self.events.subscribe()
    }

    /// Adds a logical breakpoint and returns its resolved address space and address.
    pub async fn add_breakpoint(&self, spec: BreakpointSpec) -> Result<BreakpointLocation> {
        let location = match spec {
            BreakpointSpec::Address(address) => BreakpointLocation::Virtual(address),
            BreakpointSpec::Function(name) => BreakpointLocation::Image(
                self.module_image
                    .function_named(&name)?
                    .ranges
                    .first()
                    .ok_or(Error::LocationUnavailable)?
                    .start,
            ),
        };

        self.request(|reply| Request::AddBreakpoint { location, reply })
            .await?;

        Ok(location)
    }

    /// Launches the inferior and runs until it stops or exits.
    pub async fn run(&self) -> Result<StopReason> {
        self.request(|reply| Request::Launch { reply }).await
    }

    /// Continues the stopped inferior until it stops or exits.
    pub async fn resume(&self) -> Result<StopReason> {
        self.request(|reply| Request::Continue { reply }).await
    }

    /// Reads one native 64-bit word from a stopped inferior.
    pub async fn read_word(&self, address: VirtualAddress) -> Result<u64> {
        self.request(|reply| Request::ReadWord { address, reply })
            .await
    }

    /// Resolves a linker symbol to its address in the running process.
    pub async fn runtime_address(&self, name: &str) -> Result<VirtualAddress> {
        let image_address = self.module_image.symbol_named(name)?.address;
        let loaded = self.loaded_module().await?;

        loaded.virtual_address(image_address)
    }

    /// Resolves the current stop address to normalized function and source metadata.
    pub async fn current_location(&self) -> Result<ExecutionLocation> {
        let (loaded, address) = self.stopped_location().await?;
        let image_address = loaded.image_address(address)?;

        Ok(ExecutionLocation {
            module: loaded.id,
            address,
            image: self.module_image.locate(image_address),
        })
    }

    /// Returns an immutable snapshot of the debugger's current state.
    pub async fn snapshot(&self) -> Result<StateSnapshot> {
        self.request(|reply| Request::Snapshot { reply }).await
    }

    async fn loaded_module(&self) -> Result<LoadedModule> {
        self.request(|reply| Request::LoadedModule { reply }).await
    }

    async fn stopped_location(&self) -> Result<(LoadedModule, VirtualAddress)> {
        self.request(|reply| Request::StoppedLocation { reply })
            .await
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
