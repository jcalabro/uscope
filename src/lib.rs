mod backend;
mod debug_info;
mod error;
pub(crate) mod model;
mod protocol;
mod unwind;

pub use error::{Error, Result};
pub use model::{
    AddressRange, Architecture, Backtrace, BreakpointLocation, ByteOrder, ColumnNumber,
    ExecutionLocation, FrameKind, FunctionId, FunctionInfo, ImageAddress, ImageLocation,
    LineNumber, LoadedModule, ModuleId, ModuleImage, ModuleImageId, PointerWidth,
    RegisterDescriptor, RegisterId, RegisterRole, RegisterSnapshot, RegisterValue, SourceContext,
    SourceFile, SourceFileId, SourceLine, SourceLocation, StackFrame, StackFrameId, SymbolId,
    SymbolInfo, TargetDescription, ThreadId, UnwindTermination, VirtualAddress,
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
        let debug_info = debug_info::load(&executable)?;
        let module_image = Arc::clone(&debug_info.image);
        let (requests, receiver) = mpsc::channel(REQUEST_CAPACITY);
        let shutdown_permit = requests
            .clone()
            .try_reserve_owned()
            .expect("new request channel has shutdown capacity");
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let controller = backend::spawn_controller(
            Arc::clone(&executable),
            Arc::clone(&module_image),
            debug_info.unwind,
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

    /// Lazily reads source lines surrounding the stopped instruction.
    pub async fn source_context(&self, radius: u32) -> Result<SourceContext> {
        let execution = self.current_location().await?;
        let location = execution
            .image
            .source
            .ok_or(Error::SourceLocationUnavailable)?;
        let file = self
            .module_image
            .source_file(location.file)
            .cloned()
            .expect("source location references a known file");
        let contents = tokio::fs::read_to_string(file.path.as_ref())
            .await
            .map_err(|source| Error::SourceFileRead {
                path: file.path.as_ref().clone(),
                source,
            })?;
        let all_lines: Vec<_> = contents.lines().collect();
        let line = location.line.get();
        let target = usize::try_from(line)
            .ok()
            .and_then(|line| line.checked_sub(1))
            .filter(|line| *line < all_lines.len())
            .ok_or_else(|| Error::SourceLineOutOfRange {
                path: file.path.as_ref().clone(),
                line,
            })?;
        let radius = usize::try_from(radius).expect("u32 fits in usize");
        let start = target.saturating_sub(radius);
        let end = target
            .saturating_add(radius)
            .saturating_add(1)
            .min(all_lines.len());
        let lines = all_lines[start..end]
            .iter()
            .enumerate()
            .map(|(offset, text)| SourceLine {
                number: LineNumber::new(
                    u64::try_from(start + offset + 1).expect("source line number fits u64"),
                )
                .expect("source line number is nonzero"),
                text: Arc::from(*text),
            })
            .collect::<Vec<_>>()
            .into();

        Ok(SourceContext {
            file,
            location,
            lines,
        })
    }

    /// Returns an immutable snapshot of the debugger's current state.
    pub async fn snapshot(&self) -> Result<StateSnapshot> {
        self.request(|reply| Request::Snapshot { reply }).await
    }

    /// Reconstructs the stopped thread's stack frames.
    pub async fn backtrace(&self) -> Result<Backtrace> {
        self.request(|reply| Request::Backtrace { reply }).await
    }

    /// Reads the general register set of the stopped thread.
    pub async fn registers(&self) -> Result<RegisterSnapshot> {
        self.request(|reply| Request::Registers { reply }).await
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
