mod backend;
mod debug_info;
mod error;
pub(crate) mod model;
mod protocol;
mod unwind;

pub use error::{Error, Result};
pub use model::{
    AddressRange, Architecture, Backtrace, BaseType, BaseTypeEncoding, BreakpointEntry,
    BreakpointLocation, ByteOrder, CodeInstanceId, CodeInstanceInfo, CodeInstanceKind,
    ColumnNumber, EntryProvenance, ExecutionLocation, FloatValue, FrameKind, FunctionId,
    FunctionInfo, ImageAddress, ImageLocation, InlineChain, InlineFrameLookup, LineNumber,
    LineSequenceId, LoadedModule, ModuleId, ModuleImage, ModuleImageId, PointerWidth,
    RegisterDescriptor, RegisterId, RegisterRole, RegisterSnapshot, RegisterValue, ScalarValue,
    SourceContext, SourceFile, SourceFileId, SourceLine, SourceLocation, StackFrame, StackFrameId,
    StatementFlags, StatementRow, SymbolId, SymbolInfo, TargetDescription, ThreadId,
    UnsupportedVariableFeature, UnwindTermination, Variable, VariableKind, VariableMalformedReason,
    VariableSnapshot, VariableState, VariableUnavailableReason, VariableValueSource,
    VirtualAddress,
};
pub use protocol::{
    Breakpoint, BreakpointId, BreakpointSpec, DebuggerEvent, ExceptionDisposition, ExceptionInfo,
    ExecutionId, ExitStatus, FramePresentation, InferiorState, PresentedFrame, ProcessId,
    ResolvedBreakpointLocation, ResumeScope, StateSnapshot, StepKind, StopId, StopReason,
    ThreadSnapshot, ThreadState, VariableQuery,
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
            debug_info.variables,
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

    /// Adds a logical breakpoint and returns all locations resolved by the backend.
    pub async fn add_breakpoint(&self, spec: BreakpointSpec) -> Result<Breakpoint> {
        self.request(|reply| Request::AddBreakpoint { spec, reply })
            .await
    }

    /// Removes one logical breakpoint and returns its prior definition.
    pub async fn remove_breakpoint(&self, id: BreakpointId) -> Result<Breakpoint> {
        self.request(|reply| Request::RemoveBreakpoint { id, reply })
            .await
    }

    /// Removes every logical breakpoint and returns their prior definitions.
    pub async fn remove_all_breakpoints(&self) -> Result<Arc<[Breakpoint]>> {
        self.request(|reply| Request::RemoveAllBreakpoints { reply })
            .await
    }

    /// Launches the inferior and acknowledges once native execution has started.
    pub async fn launch(&self) -> Result<ExecutionId> {
        self.request(|reply| Request::Launch { reply }).await
    }

    /// Launches the inferior and waits until that execution stops or exits.
    pub async fn run(&self) -> Result<StopReason> {
        let mut events = self.subscribe();
        let execution = self.launch().await?;

        self.wait_for_execution(&mut events, execution).await
    }

    /// Continues the stopped inferior and acknowledges once threads have resumed.
    pub async fn continue_execution(
        &self,
        stop_id: StopId,
        scope: ResumeScope,
        exception: ExceptionDisposition,
    ) -> Result<ExecutionId> {
        let process_id = match scope {
            ResumeScope::Process(process_id) => process_id,
            ResumeScope::Thread(_) => self.stopped_selection().await?.process,
        };

        self.request(|reply| Request::Continue {
            process_id,
            stop_id,
            scope,
            exception,
            reply,
        })
        .await
    }

    /// Continues every thread until the inferior stops or exits.
    pub async fn resume(&self) -> Result<StopReason> {
        self.resume_with_exception(ExceptionDisposition::Pass).await
    }

    /// Continues every thread with an explicit pending-exception disposition.
    pub async fn resume_with_exception(
        &self,
        exception: ExceptionDisposition,
    ) -> Result<StopReason> {
        let selection = self.stopped_selection().await?;
        let mut events = self.subscribe();
        let execution = self
            .continue_execution(
                selection.stop,
                ResumeScope::Process(selection.process),
                exception,
            )
            .await?;

        self.wait_for_execution(&mut events, execution).await
    }

    /// Starts a thread-specific stepping operation.
    pub async fn start_step(
        &self,
        stop_id: StopId,
        thread_id: ThreadId,
        kind: StepKind,
        exception: ExceptionDisposition,
    ) -> Result<ExecutionId> {
        let process_id = self.stopped_selection().await?.process;

        self.request(|reply| Request::Step {
            process_id,
            stop_id,
            thread_id,
            kind,
            exception,
            reply,
        })
        .await
    }

    /// Steps the selected thread and waits until the operation stops or exits.
    pub async fn step(&self, kind: StepKind) -> Result<StopReason> {
        let selection = self.stopped_selection().await?;
        let mut events = self.subscribe();
        let execution = self
            .start_step(
                selection.stop,
                selection.thread,
                kind,
                ExceptionDisposition::Pass,
            )
            .await?;

        self.wait_for_execution(&mut events, execution).await
    }

    /// Pauses a running process and waits for a coherent all-stop snapshot.
    pub async fn pause(&self) -> Result<StopReason> {
        let snapshot = self.snapshot().await?;
        let InferiorState::Running { process_id, .. } = snapshot.inferior else {
            return Err(if matches!(snapshot.inferior, InferiorState::NotRunning) {
                Error::NotRunning
            } else {
                Error::NotStopped
            });
        };
        let mut events = self.subscribe();
        let execution = self
            .request(|reply| Request::Pause { process_id, reply })
            .await?;

        self.wait_for_execution(&mut events, execution).await
    }

    /// Reads one native 64-bit word from a stopped inferior.
    pub async fn read_word(&self, address: VirtualAddress) -> Result<u64> {
        let selection = self.stopped_selection().await?;

        self.request(|reply| Request::ReadWord {
            process_id: selection.process,
            stop_id: selection.stop,
            address,
            reply,
        })
        .await
    }

    /// Writes one native 64-bit word while preserving installed debugger breakpoints.
    pub async fn write_word(&self, address: VirtualAddress, value: u64) -> Result<()> {
        let selection = self.stopped_selection().await?;

        self.request(|reply| Request::WriteWord {
            process_id: selection.process,
            stop_id: selection.stop,
            address,
            value,
            reply,
        })
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
        self.stopped_location().await
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
        let selection = self.stopped_selection().await?;

        self.request(|reply| Request::Backtrace {
            stop_id: selection.stop,
            thread_id: selection.thread,
            reply,
        })
        .await
    }

    /// Reads the general register set of the stopped thread.
    pub async fn registers(&self) -> Result<RegisterSnapshot> {
        let selection = self.stopped_selection().await?;

        self.request(|reply| Request::Registers {
            stop_id: selection.stop,
            thread_id: selection.thread,
            reply,
        })
        .await
    }

    /// Inspects every visible parameter and local variable in the selected logical frame.
    pub async fn variables(&self) -> Result<VariableSnapshot> {
        self.variable_query(VariableQuery::All).await
    }

    /// Inspects the innermost visible data object with the supplied name.
    pub async fn variable(&self, name: impl Into<String>) -> Result<Variable> {
        let name = name.into();
        let snapshot = self
            .variable_query(VariableQuery::Name(name.clone()))
            .await?;
        snapshot
            .variables
            .first()
            .cloned()
            .ok_or(Error::VariableNotFound(name))
    }

    async fn variable_query(&self, query: VariableQuery) -> Result<VariableSnapshot> {
        let selection = self.stopped_selection().await?;
        self.request(|reply| Request::Variables {
            query,
            stop_id: selection.stop,
            thread_id: selection.thread,
            reply,
        })
        .await
    }

    /// Selects the stopped thread used by implicit inspection commands.
    pub async fn select_thread(&self, thread_id: ThreadId) -> Result<()> {
        let selection = self.stopped_selection().await?;

        self.request(|reply| Request::SelectThread {
            stop_id: selection.stop,
            thread_id,
            reply,
        })
        .await
    }

    async fn loaded_module(&self) -> Result<LoadedModule> {
        self.request(|reply| Request::LoadedModule { reply }).await
    }

    async fn stopped_location(&self) -> Result<ExecutionLocation> {
        let selection = self.stopped_selection().await?;

        self.request(|reply| Request::StoppedLocation {
            stop_id: selection.stop,
            thread_id: selection.thread,
            reply,
        })
        .await
    }

    async fn stopped_selection(&self) -> Result<StoppedSelection> {
        let snapshot = self.snapshot().await?;
        let selected_thread = snapshot.selected_thread;
        let InferiorState::Stopped {
            process_id,
            stop_id,
            thread_id,
            ..
        } = snapshot.inferior
        else {
            return Err(if matches!(snapshot.inferior, InferiorState::NotRunning) {
                Error::NotRunning
            } else {
                Error::NotStopped
            });
        };

        Ok(StoppedSelection {
            process: process_id,
            stop: stop_id,
            thread: selected_thread.unwrap_or(thread_id),
        })
    }

    async fn wait_for_execution(
        &self,
        events: &mut broadcast::Receiver<DebuggerEvent>,
        execution: ExecutionId,
    ) -> Result<StopReason> {
        loop {
            match events.recv().await {
                Ok(DebuggerEvent::InferiorStopped {
                    execution_id: Some(event_execution),
                    reason,
                    ..
                }) if event_execution == execution => return Ok(reason),
                Ok(DebuggerEvent::InferiorExited {
                    execution_id: Some(event_execution),
                    status,
                    ..
                }) if event_execution == execution => return Ok(StopReason::Exited(status)),
                Ok(_) => {}
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(Error::RequestCancelled);
                }
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    return Err(Error::EventStreamLagged(count));
                }
            }
        }
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

#[derive(Clone, Copy)]
struct StoppedSelection {
    process: ProcessId,
    stop: StopId,
    thread: ThreadId,
}
