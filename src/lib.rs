/// Appends a line to the flight recorder of a development build. Release
/// builds drop the arguments unevaluated, so they may use code that only
/// development builds compile.
macro_rules! record {
    ($($argument:tt)*) => {{
        #[cfg(debug_assertions)]
        $crate::flight_recorder::record(::std::format_args!($($argument)*));
    }};
}

mod backend;
mod condition;
mod debug_info;
mod demangle;
mod disassembly;
mod error;
#[allow(
    dead_code,
    reason = "the evaluator is wired in by a later phase of plans/expressions.md"
)]
mod eval;
#[cfg(debug_assertions)]
#[doc(hidden)]
pub mod flight_recorder;
mod inspection;
pub(crate) mod model;
mod protocol;
#[cfg(any(test, feature = "sim"))]
#[doc(hidden)]
pub mod sim;
mod source_map;
#[cfg(test)]
mod test_memory;
mod type_identity;
mod unwind;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::timeout;

use backend::ControllerMessage;
use protocol::Request;

pub use condition::{Condition, LogMessage, LogSegment};
pub use disassembly::{
    AssemblySyntax, BlockCompletion, BoundaryConflict, BoundaryEvidence, ContextShortfall,
    ControlFlow, DecodedInstruction, DisassembledFunction, DisassembledInstruction, Disassembly,
    DisassemblyBlock, DisassemblyQuery, DisassemblyRange, DisassemblyView, FunctionOrigin,
    IndirectTarget, InstructionContent, InstructionReference, InstructionReferenceKind,
    InstructionToken, InstructionTokenKind, MAX_BACKWARD_DISTANCE, MAX_FUNCTION_INSTRUCTIONS,
    MAX_WINDOW_AFTER, MAX_WINDOW_BEFORE, TargetBoundary,
};
pub use error::{Error, Result};
pub use eval::Evaluation;
pub use eval::bind::Mode as EvaluationMode;
pub use eval::error::{ErrorKind as ExpressionErrorKind, ExpressionError};
pub use eval::syntax::{Expression, Span};
pub use model::{
    Accessibility, AddressDescription, AddressRange, AddressValue, Architecture, ArgumentOrigin,
    ArrayDimension, Backtrace, BaseClass, BaseClassVirtuality, BaseType, BaseTypeEncoding,
    BreakpointEntry, BreakpointLocation, ByteOrder, CallFrameUnavailableReason, CodeInstanceId,
    CodeInstanceInfo, CodeInstanceKind, ColumnNumber, DereferenceReference, DereferenceState,
    DereferenceUnavailableReason, DereferencedValue, EmbeddedSymbolTable, EntryProvenance,
    EnumerationOrigin, Enumerator, ExecutionLocation, FloatValue, FrameKind, FunctionId,
    FunctionInfo, GlobalVariableCandidate, GlobalVariableId, GlobalVariableInfo,
    GlobalVariablePage, GlobalVariableReference, GlobalVariableType, GlobalVariableVisibility,
    GoKind, GoTypeAttributes, ImageAddress, ImageAddressDescription, ImageLocation, InlineChain,
    InlineFrameLookup, InspectedValue, InspectionCompletion, InspectionExhaustion, InspectionLimit,
    InspectionLimits, InspectionUsage, IntegerValue, LineNumber, LineSequenceId,
    LoadedGlobalVariableInfo, LoadedModule, LoadedModuleRecord, LoadedModuleSnapshot, MemoryRead,
    MemoryReadCompletion, MemoryReadUnavailableReason, ModuleAddress, ModuleId, ModuleImage,
    ModuleImageId, NamedTypeRelationship, OptimizedOutReason, PointerWidth, RecordKind,
    RecordMember, RecordMemberLayout, ReferenceKind, RegisterDescriptor, RegisterId, RegisterRole,
    RegisterSnapshot, RegisterValue, ScalarValue, SectionId, SectionInfo, SectionLocation,
    SourceContext, SourceFile, SourceFileId, SourceLanguage, SourceLine, SourceLocation,
    StackFrame, StackFrameId, StatementFlags, StatementRow, SymbolBinding, SymbolExtent,
    SymbolExtentProvenance, SymbolId, SymbolInfo, SymbolKind, SymbolLocation, SymbolTableSources,
    TargetDescription, TextCompletion, TextSummary, ThreadId, TlsUnavailableReason, TypeArgument,
    TypeId, TypeIdentity, TypeInfo, TypeKind, TypeModifier, TypeNode, TypeReference,
    UnsupportedVariableFeature, UnwindTermination, ValueAccessUnavailableReason, ValueBitRange,
    ValueChild, ValueChildPage, ValueChildRelationship, ValueChildren, ValueChildrenReference,
    ValuePageCompletion, Variable, VariableInvalidReason, VariableKind, VariableMalformedKind,
    VariableMalformedReason, VariableSnapshot, VariableState, VariableUnavailableReason,
    VariableValue, VariableValueSource, Variant, VariantDiscriminant, VariantSelection,
    VariantSelector, VariantStorageKind, VirtualAddress,
};
pub use protocol::{
    Breakpoint, BreakpointHit, BreakpointId, BreakpointOptions, BreakpointSpec, CoreDumpInfo,
    CoreDumpOptions, CoreModule, CoreModuleState, DebuggerEvent, ExceptionDisposition,
    ExceptionInfo, ExecutionId, ExitStatus, FramePresentation, GlobalVariableQuery, HitComparison,
    HitCondition, InferiorState, InvalidatedWatchpoint, LaunchOptions, LogPart, ModuleIdentity,
    PresentedFrame, ProcessId, ResolvedBreakpointLocation, ResumeScope, SignalPolicy,
    StateSnapshot, StepKind, StopId, StopReason, ThreadSnapshot, ThreadState, ValueChildQuery,
    VariableQuery, WatchAccess, WatchScope, WatchTarget, Watchpoint, WatchpointCapabilities,
    WatchpointHit, WatchpointId, WatchpointInvalidation, WatchpointSpec,
};
pub use source_map::SourcePathMap;

/// Finds a signal's exception code by name, with or without its `SIG`
/// prefix and in any case, or by number: `SIGUSR1`, `usr1`, `10`, `SIG34`.
#[must_use]
pub fn signal_named(name: &str) -> Option<u64> {
    backend::signal_named(name)
}

/// Names the signal with an exception code, such as `SIGSEGV`. Real-time
/// signals are named by number, as gdb does: `SIG34`.
#[must_use]
pub fn signal_name(code: u64) -> Option<String> {
    backend::signal_name(code)
}

/// The exception codes of every signal this target defines, in order.
pub fn signal_codes() -> impl Iterator<Item = u64> {
    backend::signal_codes()
}

/// Makes every TLS lookup in this process compute addresses from glibc's own
/// layout descriptors instead of asking `libthread_db`, which is otherwise
/// used whenever it accepts the inferior's C library. Like gdb's `maint set
/// force-internal-tls-address-lookup`, this exists to test that the two agree.
#[doc(hidden)]
pub fn force_internal_tls_lookup(forced: bool) {
    backend::force_internal_tls_lookup(forced);
}

/// Exercises core-dump parsing and memory reads for the fuzz harness.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_core_dump(data: &[u8]) {
    backend::fuzz_core_dump(data);
}

/// Exercises debug-register planning invariants for the fuzz harness.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_debug_register_plan(data: &[u8]) {
    backend::fuzz_debug_register_plan(data);
}

/// Exercises ELF symbol-table normalization and symbol lookup invariants for
/// the fuzz harness.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_elf_symbols(data: &[u8]) {
    debug_info::fuzz_elf_symbols(data);
}

/// Exercises disassembly boundary and decoding invariants for the fuzz
/// harness.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_disassembly(data: &[u8]) {
    disassembly::fuzz(data);
}

/// Checks the expression parser's invariants on `text` for the fuzz
/// harness, panicking when one fails.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_expression_parse(text: &str) {
    if let Err(failure) = eval::syntax::check_invariants(text) {
        panic!("{failure}");
    }
}

/// Exercises bounded DWARF-expression parsing for the fuzz harness.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_dwarf_expression(data: &[u8]) {
    debug_info::fuzz_dwarf_expression(data);
}

const REQUEST_CAPACITY: usize = 32;
const EVENT_CAPACITY: usize = 256;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// The number of events a subscriber may fall behind by before it misses
/// some. `USCOPE_EVENT_CAPACITY` lowers it so tests can make clients lag.
fn event_capacity() -> usize {
    std::env::var("USCOPE_EVENT_CAPACITY")
        .ok()
        .and_then(|capacity| capacity.parse().ok())
        .filter(|capacity| *capacity != 0)
        .unwrap_or(EVENT_CAPACITY)
}

/// A debug session, owning the controller thread that serves its requests.
///
/// Dropping a debugger requests shutdown without waiting for it; prefer
/// [`Debugger::shutdown`], which reports whether cleanup succeeded.
pub struct Debugger {
    handle: DebuggerHandle,
    controller: Option<JoinHandle<()>>,
    shutdown_permit: Option<mpsc::OwnedPermit<ControllerMessage>>,
}

/// A clonable client of a running [`Debugger`].
///
/// Requests are queued to the controller, which acknowledges each one.
/// Execution control is acknowledged before the stop it eventually causes;
/// subscribe to events, or use the methods that wait, to observe the stop.
#[derive(Clone)]
pub struct DebuggerHandle {
    module_image: Arc<ModuleImage>,
    core_dump: Option<Arc<CoreDumpInfo>>,
    source_paths: Arc<SourcePathMap>,
    requests: mpsc::Sender<ControllerMessage>,
    events: broadcast::Sender<DebuggerEvent>,
}

impl Debugger {
    /// Creates a debugger for a native executable and starts its backend controller.
    pub fn new(executable: impl AsRef<Path>) -> Result<Self> {
        let executable = backend::executable_source(executable.as_ref())?;
        Self::from_executable_source(executable)
    }

    /// Attaches to an existing local process and returns once it is coherently stopped.
    pub async fn attach(process: ProcessId) -> Result<Self> {
        let executable = backend::process_executable_source(process)?;
        Self::attach_from_source(process, executable).await
    }

    /// Attaches using an explicitly supplied executable when automatic discovery is unavailable.
    pub async fn attach_with_executable(
        process: ProcessId,
        executable: impl AsRef<Path>,
    ) -> Result<Self> {
        let executable = backend::executable_source(executable.as_ref())?;
        Self::attach_from_source(process, executable).await
    }

    async fn attach_from_source(
        process: ProcessId,
        executable: backend::ExecutableSource,
    ) -> Result<Self> {
        let debugger = Self::from_executable_source(executable)?;
        if let Err(error) = debugger.handle.attach_process(process).await {
            let _ = debugger.shutdown().await;
            return Err(error);
        }
        Ok(debugger)
    }

    /// Opens a post-mortem core dump as one permanent stopped snapshot.
    ///
    /// Every recorded module file must be proven to match the dump unless
    /// [`CoreDumpOptions::allow_module_mismatch`] is set. The files of a dump
    /// written on another machine are found through
    /// [`CoreDumpOptions::sysroot`] and [`CoreDumpOptions::module_paths`].
    /// Execution control, memory writes, and breakpoints fail with
    /// [`Error::PostMortemTarget`].
    pub fn open_core(options: &CoreDumpOptions) -> Result<Self> {
        Self::start(|channels| {
            let session = backend::open_core(options, channels)?;
            Ok((session.image, Some(session.info), session.controller))
        })
    }

    fn from_executable_source(executable: backend::ExecutableSource) -> Result<Self> {
        let debug_info = debug_info::load_bytes(&executable.display_path, &executable.data)?;
        Self::start(|channels| {
            let image = Arc::clone(&debug_info.image);
            let controller = backend::spawn_controller(executable, debug_info, channels)?;
            Ok((image, None, controller))
        })
    }

    /// Creates the request and event channels and starts a controller that
    /// serves them, reserving request capacity so shutdown can always be sent.
    fn start(
        spawn: impl FnOnce(
            backend::ControllerChannels,
        )
            -> Result<(Arc<ModuleImage>, Option<Arc<CoreDumpInfo>>, JoinHandle<()>)>,
    ) -> Result<Self> {
        #[cfg(debug_assertions)]
        flight_recorder::record_panics();
        let (requests, receiver) = mpsc::channel(REQUEST_CAPACITY);
        let shutdown_permit = requests
            .clone()
            .try_reserve_owned()
            .expect("new request channel has shutdown capacity");
        let (events, _) = broadcast::channel(event_capacity());
        let (module_image, core_dump, controller) = spawn(backend::ControllerChannels {
            sender: requests.clone(),
            receiver,
            events: events.clone().into(),
        })?;

        Ok(Self {
            handle: DebuggerHandle {
                module_image,
                core_dump,
                source_paths: Arc::default(),
                requests,
                events,
            },
            controller: Some(controller),
            shutdown_permit: Some(shutdown_permit),
        })
    }

    /// Returns a clonable handle for sending requests to the debugger.
    #[must_use]
    pub fn handle(&self) -> DebuggerHandle {
        self.handle.clone()
    }

    /// Ends the session and joins the backend controller. A launched inferior
    /// is killed and reaped; an attached one is detached and left running.
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
    /// Returns this handle reading source files through `source_paths`.
    /// Other handles of the same debugger keep their own maps.
    #[must_use]
    pub fn with_source_paths(mut self, source_paths: SourcePathMap) -> Self {
        self.source_paths = Arc::new(source_paths);
        self
    }

    /// Returns the canonical path of the executable being debugged.
    #[must_use]
    pub fn executable(&self) -> &Path {
        self.module_image.path()
    }

    /// Returns the immutable debug metadata for the main executable.
    #[must_use]
    pub const fn module_image(&self) -> &Arc<ModuleImage> {
        &self.module_image
    }

    /// Describes the opened core dump, or `None` for a live session.
    #[must_use]
    pub const fn core_dump(&self) -> Option<&Arc<CoreDumpInfo>> {
        self.core_dump.as_ref()
    }

    /// Subscribes to debugger state and lifecycle events.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<DebuggerEvent> {
        self.events.subscribe()
    }

    /// Adds a logical breakpoint that stops at every hit and returns all
    /// locations resolved by the backend. Adding the spec of an existing
    /// breakpoint without a hit condition returns that breakpoint.
    pub async fn add_breakpoint(&self, spec: BreakpointSpec) -> Result<Breakpoint> {
        self.add_breakpoint_with(spec, BreakpointOptions::default())
            .await
    }

    /// Adds a logical breakpoint that stops or logs at the hits `options`
    /// select. Adding the spec and options of an existing breakpoint
    /// returns that breakpoint, with the hits it already counted.
    pub async fn add_breakpoint_with(
        &self,
        spec: BreakpointSpec,
        options: BreakpointOptions,
    ) -> Result<Breakpoint> {
        self.request(|reply| Request::AddBreakpoint {
            spec,
            options: Box::new(options),
            reply,
        })
        .await
    }

    /// Adds a logical breakpoint that stops only at hits meeting
    /// `hit_condition`. Adding the spec and condition of an existing
    /// breakpoint returns that breakpoint, with the hits it already counted.
    pub async fn add_breakpoint_with_hit_condition(
        &self,
        spec: BreakpointSpec,
        hit_condition: HitCondition,
    ) -> Result<Breakpoint> {
        self.add_breakpoint_with(
            spec,
            BreakpointOptions {
                hit_condition: Some(hit_condition),
                ..BreakpointOptions::default()
            },
        )
        .await
    }

    /// Replaces which hits of a breakpoint stop execution; `None` stops at
    /// every hit. The hits already counted are kept. Unlike adding and
    /// removing breakpoints, this needs no stop and works while the inferior
    /// runs, taking effect from the next hit.
    pub async fn set_breakpoint_hit_condition(
        &self,
        id: BreakpointId,
        hit_condition: Option<HitCondition>,
    ) -> Result<Breakpoint> {
        self.request(|reply| Request::SetBreakpointHitCondition {
            id,
            hit_condition,
            reply,
        })
        .await
    }

    /// Replaces a breakpoint's condition; `None` removes it. Like a hit
    /// condition, this needs no stop and applies from the next hit.
    pub async fn set_breakpoint_condition(
        &self,
        id: BreakpointId,
        condition: Option<Condition>,
    ) -> Result<Breakpoint> {
        self.request(|reply| Request::SetBreakpointCondition {
            id,
            condition,
            reply,
        })
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

    /// Describes what this platform's debug hardware can watch.
    #[must_use]
    pub fn watchpoint_capabilities(&self) -> WatchpointCapabilities {
        backend::watchpoint_capabilities()
    }

    /// Resolves an expression in the selected thread's selected frame to the
    /// memory it occupies and the lifetime of that storage.
    pub async fn resolve_watch_target(&self, expression: &Expression) -> Result<WatchTarget> {
        self.selected()
            .await?
            .resolve_watch_target(expression)
            .await
    }

    /// Arms a hardware watchpoint on every thread of the stopped process.
    ///
    /// Arming is atomic: on failure no thread is left armed and no event is
    /// published.
    pub async fn add_watchpoint(
        &self,
        spec: WatchpointSpec,
        access: WatchAccess,
    ) -> Result<Watchpoint> {
        self.request(|reply| Request::AddWatchpoint {
            spec,
            access,
            reply,
        })
        .await
    }

    /// Resolves an expression at the current stop and watches its memory.
    pub async fn watch(&self, expression: &Expression, access: WatchAccess) -> Result<Watchpoint> {
        let target = self.resolve_watch_target(expression).await?;
        self.add_watchpoint(WatchpointSpec::Target(Box::new(target)), access)
            .await
    }

    /// Disarms one watchpoint and returns its prior definition.
    pub async fn remove_watchpoint(&self, id: WatchpointId) -> Result<Watchpoint> {
        self.request(|reply| Request::RemoveWatchpoint { id, reply })
            .await
    }

    /// Disarms every watchpoint and returns their prior definitions.
    pub async fn remove_all_watchpoints(&self) -> Result<Arc<[Watchpoint]>> {
        self.request(|reply| Request::RemoveAllWatchpoints { reply })
            .await
    }

    /// Returns how the debugger handles a signal, named by its exception
    /// code.
    pub async fn signal_policy(&self, signal: u64) -> Result<SignalPolicy> {
        self.request(|reply| Request::SignalPolicy { signal, reply })
            .await
    }

    /// Changes how the debugger handles a signal and returns the previous
    /// policy. The change applies at once, also while the inferior runs, and
    /// lasts for the whole session.
    pub async fn set_signal_policy(
        &self,
        signal: u64,
        policy: SignalPolicy,
    ) -> Result<SignalPolicy> {
        self.request(|reply| Request::SetSignalPolicy {
            signal,
            policy,
            reply,
        })
        .await
    }

    /// Kills the inferior and waits until it is gone, keeping the session:
    /// the program can be launched again, or another process attached.
    pub async fn kill(&self) -> Result<()> {
        self.request(|reply| Request::Kill { reply }).await
    }

    /// Asks the inferior to end, as with `SIGTERM` on Linux, and returns once
    /// the request is sent. The request itself never stops the inferior,
    /// whatever the signal's policy, and a stopped inferior is resumed to
    /// receive it; the program then exits, or stops for a breakpoint or
    /// signal while ending, as events report.
    pub async fn terminate(&self) -> Result<()> {
        self.request(|reply| Request::Terminate { reply }).await
    }

    /// Launches the inferior and acknowledges once native execution has started.
    pub async fn launch(&self) -> Result<ExecutionId> {
        self.launch_with(LaunchOptions::default()).await
    }

    /// Launches the inferior as `options` describe and acknowledges once
    /// native execution has started, or once it stopped at its entry.
    pub async fn launch_with(&self, options: LaunchOptions) -> Result<ExecutionId> {
        self.request(|reply| Request::Launch {
            options: Box::new(options),
            reply,
        })
        .await
    }

    /// Launches through a process that is waiting to exec this debugger's
    /// executable, such as a launcher started in a terminal, and
    /// acknowledges as [`Self::launch_with`] does once it has.
    ///
    /// The process must be single-threaded. `release` runs once it is
    /// traced and must then let it exec; until it does, the process runs
    /// and receives signals as it would untraced. Exiting first fails the
    /// launch.
    pub async fn launch_by_exec(
        &self,
        process_id: ProcessId,
        stop_at_entry: bool,
        release: impl FnOnce() + Send + 'static,
    ) -> Result<ExecutionId> {
        self.request(|reply| Request::LaunchByExec {
            process_id,
            stop_at_entry,
            release: Box::new(release),
            reply,
        })
        .await
    }

    /// Attaches to an existing process and returns its coherent initial stop.
    pub async fn attach_process(&self, process_id: ProcessId) -> Result<StopId> {
        self.request(|reply| Request::Attach { process_id, reply })
            .await
    }

    /// Launches the inferior and waits until that execution stops or exits.
    pub async fn run(&self) -> Result<StopReason> {
        self.run_with(LaunchOptions::default()).await
    }

    /// Launches the inferior as `options` describe and waits until that
    /// execution stops or exits.
    pub async fn run_with(&self, options: LaunchOptions) -> Result<StopReason> {
        let mut events = self.subscribe();
        let execution = self.launch_with(options).await?;

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

    /// Starts stepping one thread.
    ///
    /// [`StepKind::Out`] runs until `frame` returns to its caller; every
    /// other kind steps from the innermost frame, which `frame` must be.
    ///
    /// With [`ResumeScope::Process`], every other thread runs while the step
    /// does, so a step over a call that waits for another thread completes.
    /// Another thread's stop, such as a breakpoint or a signal, then ends
    /// the step before it completes. [`ResumeScope::Thread`] naming the
    /// stepping thread keeps every other thread stopped.
    pub async fn start_step(
        &self,
        stop_id: StopId,
        thread_id: ThreadId,
        frame: StackFrameId,
        kind: StepKind,
        scope: ResumeScope,
        exception: ExceptionDisposition,
    ) -> Result<ExecutionId> {
        let process_id = self.stopped_selection().await?.process;

        self.request(|reply| Request::Step {
            process_id,
            stop_id,
            thread_id,
            frame,
            kind,
            scope,
            exception,
            reply,
        })
        .await
    }

    /// Steps the selected thread, running every other thread too, and waits
    /// until the operation stops or exits.
    ///
    /// Stepping out leaves the selected frame; every other step begins at
    /// the innermost frame, whichever frame is selected.
    pub async fn step(&self, kind: StepKind) -> Result<StopReason> {
        self.step_with_exception(kind, ExceptionDisposition::Pass)
            .await
    }

    /// Steps the selected thread with an explicit pending-exception disposition.
    pub async fn step_with_exception(
        &self,
        kind: StepKind,
        exception: ExceptionDisposition,
    ) -> Result<StopReason> {
        let selection = self.stopped_selection().await?;
        let frame = if kind == StepKind::Out {
            selection.frame
        } else {
            StackFrameId::INNERMOST
        };
        let mut events = self.subscribe();
        let execution = self
            .start_step(
                selection.stop,
                selection.thread,
                frame,
                kind,
                ResumeScope::Process(selection.process),
                exception,
            )
            .await?;

        self.wait_for_execution(&mut events, execution).await
    }

    /// Pauses a running process and waits for a coherent all-stop snapshot.
    ///
    /// A process that is still launching stops at its initial exec stop
    /// instead of running first.
    pub async fn pause(&self) -> Result<StopReason> {
        if self.core_dump.is_some() {
            return Err(Error::PostMortemTarget);
        }
        let snapshot = self.snapshot().await?;
        let InferiorState::Running { process_id, .. } = snapshot.inferior else {
            return Err(if matches!(snapshot.inferior, InferiorState::NotRunning) {
                Error::NotRunning
            } else {
                Error::AlreadyStopped
            });
        };
        let mut events = self.subscribe();
        let execution = self
            .request(|reply| Request::Pause { process_id, reply })
            .await?;

        self.wait_for_execution(&mut events, execution).await
    }

    /// Reads a bounded byte range from a stopped inferior.
    ///
    /// Readable bytes are returned as one contiguous prefix. Target memory
    /// becoming inaccessible is represented by `MemoryRead::completion`;
    /// debugger and process-control failures remain errors.
    pub async fn read_memory(
        &self,
        address: VirtualAddress,
        byte_count: u64,
    ) -> Result<MemoryRead> {
        let selection = self.stopped_selection().await?;

        self.request(|reply| Request::ReadMemory {
            process_id: selection.process,
            stop_id: selection.stop,
            address,
            byte_count,
            reply,
        })
        .await
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

    /// Writes bytes into a stopped inferior and returns how many were
    /// written: all of them, or those before memory that cannot be written,
    /// which fails when it is the first. Debugger breakpoint traps stay in
    /// place, hiding the written bytes as they hid the old ones.
    pub async fn write_memory(&self, address: VirtualAddress, bytes: &[u8]) -> Result<u64> {
        let selection = self.stopped_selection().await?;

        self.request(|reply| Request::WriteMemory {
            process_id: selection.process,
            stop_id: selection.stop,
            address,
            bytes: bytes.into(),
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

    /// Describes a process address by the loaded module, section, and symbol
    /// containing it at the current stop.
    ///
    /// A code address is named by the symbol whose extent contains it, and a
    /// data address only by a symbol whose declared storage contains it; the
    /// nearest preceding symbol is never substituted.
    pub async fn describe_address(&self, address: VirtualAddress) -> Result<AddressDescription> {
        let selection = self.stopped_selection().await?;

        self.request(|reply| Request::DescribeAddress {
            stop_id: selection.stop,
            address,
            reply,
        })
        .await
    }

    /// Disassembles code at the current stop as the selected thread sees it.
    ///
    /// Bytes come from the stopped process or core dump with debugger
    /// breakpoint traps hidden. Instructions are decoded only forward from
    /// proven instruction starts, among them the selected thread's program
    /// counter; see [`Disassembly`] for how unproven, unreadable, and
    /// conflicting code is reported. Indirect branches name the targets the
    /// stopped state gives them; see [`IndirectTarget`].
    pub async fn disassemble(&self, query: DisassemblyQuery) -> Result<Disassembly> {
        self.selected().await?.disassemble(query).await
    }

    /// Resolves the selected frame's location to normalized function and
    /// source metadata: where execution stopped in the innermost frame, and
    /// the call in progress in an outer one.
    pub async fn current_location(&self) -> Result<ExecutionLocation> {
        self.selected().await?.location().await
    }

    /// Lazily reads source lines surrounding the selected frame's location,
    /// from the first place this handle's [`SourcePathMap`] finds the file.
    pub async fn source_context(&self, radius: u32) -> Result<SourceContext> {
        self.selected().await?.source_context(radius).await
    }

    /// Reads source lines surrounding a frame's location.
    async fn source_context_at(
        &self,
        execution: ExecutionLocation,
        radius: u32,
    ) -> Result<SourceContext> {
        let location = execution
            .image
            .source
            .ok_or(Error::SourceLocationUnavailable)?;

        // Source files are identified within the image of the module that
        // contains the frame's code.
        let file = self
            .loaded_module_image(execution.module)
            .await?
            .source_file(location.file)
            .cloned()
            .ok_or(Error::SourceLocationUnavailable)?;

        let (path, contents) = self.read_source(&file.path).await?;

        let all_lines: Vec<_> = contents.lines().collect();
        let line = location.line.get();
        let target = usize::try_from(line)
            .ok()
            .and_then(|line| line.checked_sub(1))
            .filter(|line| *line < all_lines.len())
            .ok_or_else(|| Error::SourceLineOutOfRange {
                path: path.clone(),
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
            path: Arc::new(path),
            location,
            lines,
        })
    }

    /// Reads the first candidate for a recorded source path that exists. A
    /// file that exists but cannot be read is an error rather than skipped.
    async fn read_source(&self, recorded: &Path) -> Result<(PathBuf, String)> {
        let candidates = self.source_paths.candidates(recorded);
        for candidate in &candidates {
            match tokio::fs::read_to_string(candidate).await {
                Ok(contents) => return Ok((candidate.clone(), contents)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(Error::SourceFileRead {
                        path: candidate.clone(),
                        error,
                    });
                }
            }
        }
        Err(Error::SourceFileMissing {
            path: recorded.to_owned(),
            tried: candidates,
        })
    }

    /// Returns an immutable snapshot of the debugger's current state.
    pub async fn snapshot(&self) -> Result<StateSnapshot> {
        self.request(|reply| Request::Snapshot { reply }).await
    }

    /// Reconstructs the selected thread's stack frames.
    pub async fn backtrace(&self) -> Result<Backtrace> {
        self.selected().await?.backtrace().await
    }

    /// Reads the general register set of the selected frame.
    pub async fn registers(&self) -> Result<RegisterSnapshot> {
        self.selected().await?.registers().await
    }

    /// Inspects every visible parameter and local variable in the selected frame.
    pub async fn variables(&self) -> Result<VariableSnapshot> {
        self.variables_with_limits(InspectionLimits::default())
            .await
    }

    /// Inspects visible variables under explicit bounded resource limits.
    pub async fn variables_with_limits(
        &self,
        limits: InspectionLimits,
    ) -> Result<VariableSnapshot> {
        self.variable_query(VariableQuery::All, limits).await
    }

    /// Inspects the innermost visible data object with the supplied name.
    pub async fn variable(&self, name: impl Into<String>) -> Result<Variable> {
        self.variable_with_limits(name, InspectionLimits::default())
            .await
    }

    /// Inspects one named data object under explicit bounded resource limits.
    pub async fn variable_with_limits(
        &self,
        name: impl Into<String>,
        limits: InspectionLimits,
    ) -> Result<Variable> {
        let name = name.into();
        let snapshot = self
            .variable_query(VariableQuery::Name(name.clone()), limits)
            .await?;
        snapshot
            .variables
            .first()
            .cloned()
            .ok_or(Error::VariableNotFound(name))
    }

    /// Evaluates an expression in the selected frame of the selected
    /// stopped thread, reading only.
    pub async fn evaluate(&self, expression: &Expression) -> Result<Evaluation> {
        self.selected().await?.evaluate(expression).await
    }

    /// Evaluates an expression in the selected frame, assigning when `mode`
    /// allows, under explicit resource limits.
    pub async fn evaluate_with(
        &self,
        expression: &Expression,
        mode: EvaluationMode,
        limits: InspectionLimits,
    ) -> Result<Evaluation> {
        self.selected()
            .await?
            .evaluate_with(expression, mode, limits)
            .await
    }

    /// The type an expression has in the selected frame, reading no memory.
    pub async fn expression_type(&self, expression: &Expression) -> Result<TypeInfo> {
        self.selected().await?.expression_type(expression).await
    }

    /// Evaluates an expression in the selected frame of the selected stopped
    /// thread and returns its value; see [`StopView::inspect`].
    pub async fn inspect(&self, expression: &Expression) -> Result<InspectedValue> {
        self.inspect_with_limits(expression, InspectionLimits::default())
            .await
    }

    /// Evaluates an expression for its value under explicit resource limits.
    pub async fn inspect_with_limits(
        &self,
        expression: &Expression,
        limits: InspectionLimits,
    ) -> Result<InspectedValue> {
        self.selected()
            .await?
            .inspect_with_limits(expression, limits)
            .await
    }

    /// Inspects one exact global catalog entry owned by the main executable
    /// image in the selected stopped thread.
    ///
    /// A [`GlobalVariableId`] is only unique within its owning [`ModuleImage`],
    /// so this convenience method is restricted to the main image. To inspect a
    /// global belonging to a shared library, resolve its owning module and pass
    /// the full [`GlobalVariableReference`] to [`Self::loaded_global`].
    pub async fn main_global(&self, id: GlobalVariableId) -> Result<Variable> {
        self.main_global_with_limits(id, InspectionLimits::default())
            .await
    }

    /// Inspects one main-image global under explicit bounded resource limits.
    pub async fn main_global_with_limits(
        &self,
        id: GlobalVariableId,
        limits: InspectionLimits,
    ) -> Result<Variable> {
        let module = self.loaded_module().await?;
        self.loaded_global_with_limits(
            GlobalVariableReference {
                module: module.id,
                image: module.image,
                variable: id,
            },
            limits,
        )
        .await
    }

    /// Inspects one exact global in a specific loaded module.
    pub async fn loaded_global(&self, global: GlobalVariableReference) -> Result<Variable> {
        self.loaded_global_with_limits(global, InspectionLimits::default())
            .await
    }

    /// Inspects one exact loaded global under explicit bounded resource limits.
    pub async fn loaded_global_with_limits(
        &self,
        global: GlobalVariableReference,
        limits: InspectionLimits,
    ) -> Result<Variable> {
        let snapshot = self
            .variable_query(VariableQuery::Global(global), limits)
            .await?;
        snapshot
            .variables
            .first()
            .cloned()
            .ok_or_else(|| Error::VariableNotFound(global.variable.to_string()))
    }

    /// Explicitly dereferences a pointer or reference value produced at the
    /// current stopped snapshot.
    pub async fn dereference(&self, reference: DereferenceReference) -> Result<DereferencedValue> {
        self.dereference_with_limits(reference, InspectionLimits::default())
            .await
    }

    /// Dereferences a value under explicit bounded resource limits.
    pub async fn dereference_with_limits(
        &self,
        reference: DereferenceReference,
        limits: InspectionLimits,
    ) -> Result<DereferencedValue> {
        self.request(|reply| Request::Dereference {
            reference,
            limits,
            reply,
        })
        .await
    }

    /// Evaluates one arbitrary bounded page from an aggregate child capability.
    pub async fn value_children(
        &self,
        reference: Arc<ValueChildrenReference>,
        query: ValueChildQuery,
    ) -> Result<ValueChildPage> {
        self.value_children_with_limits(reference, query, InspectionLimits::default())
            .await
    }

    /// Evaluates one child page under explicit bounded resource limits.
    pub async fn value_children_with_limits(
        &self,
        reference: Arc<ValueChildrenReference>,
        query: ValueChildQuery,
        limits: InspectionLimits,
    ) -> Result<ValueChildPage> {
        self.request(|reply| Request::ValueChildren {
            reference,
            query,
            limits,
            reply,
        })
        .await
    }

    /// Lists one filtered, bounded page of immutable global metadata.
    pub async fn globals(&self, query: GlobalVariableQuery) -> Result<GlobalVariablePage> {
        self.request(|reply| Request::Globals { query, reply })
            .await
    }

    /// Returns the process-wide loaded-module registry.
    pub async fn loaded_modules(&self) -> Result<LoadedModuleSnapshot> {
        self.request(|reply| Request::LoadedModules { reply }).await
    }

    /// Returns the immutable debug metadata of one loaded module, such as the
    /// module owning a backtrace frame.
    pub async fn loaded_module_image(&self, module: ModuleId) -> Result<Arc<ModuleImage>> {
        self.request(|reply| Request::ModuleImage { module, reply })
            .await
    }

    async fn variable_query(
        &self,
        query: VariableQuery,
        limits: InspectionLimits,
    ) -> Result<VariableSnapshot> {
        self.selected().await?.variable_query(query, limits).await
    }

    /// Selects a frame of the selected thread, numbered as
    /// [`Self::backtrace`] presents it, for implicit inspection commands:
    /// variables, expressions, watch targets, the current location and its
    /// source, and stepping out. Every new stop selects the innermost frame.
    ///
    /// Values in an outer frame come from the registers its callees saved.
    /// A register a callee may overwrite without saving it is unknown there,
    /// so a value held in one is
    /// [unavailable](VariableUnavailableReason::RegisterNotSaved) rather
    /// than read from the stopped thread.
    pub async fn select_frame(&self, frame: StackFrameId) -> Result<StackFrame> {
        let selection = self.stopped_selection().await?;

        self.request(|reply| Request::SelectFrame {
            stop_id: selection.stop,
            thread_id: selection.thread,
            frame,
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

    /// Returns a view of the selected frame of the selected thread at the
    /// current stop.
    async fn selected(&self) -> Result<StopView<'_>> {
        let selection = self.stopped_selection().await?;
        Ok(self.at(StopContext {
            stop: selection.stop,
            thread: selection.thread,
            frame: selection.frame,
        }))
    }

    /// Inspects one frame of one thread at one stop, independently of the
    /// selected thread and frame. Every request through the view fails with
    /// [`Error::StaleStop`] once execution has left that stop.
    #[must_use]
    pub const fn at(&self, context: StopContext) -> StopView<'_> {
        StopView {
            handle: self,
            context,
        }
    }

    async fn stopped_selection(&self) -> Result<StoppedSelection> {
        self.request(|reply| Request::StoppedSelection { reply })
            .await
    }

    /// Waits for the stop or exit that ends `execution`.
    ///
    /// The wait is cancelled if the inferior is detached first, such as by a
    /// concurrent shutdown, or if the controller exits. Every handle keeps
    /// the event channel open, so the controller's request queue closing is
    /// what reveals its exit.
    async fn wait_for_execution(
        &self,
        events: &mut broadcast::Receiver<DebuggerEvent>,
        execution: ExecutionId,
    ) -> Result<StopReason> {
        loop {
            let event = tokio::select! {
                biased;
                event = events.recv() => event,
                () = self.requests.closed() => return Err(Error::RequestCancelled),
            };
            match event {
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
                Ok(DebuggerEvent::InferiorDetached { .. })
                | Err(broadcast::error::RecvError::Closed) => return Err(Error::RequestCancelled),
                Ok(_) => {}
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

/// Names one frame of one thread at one stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopContext {
    /// The stop the frame belongs to.
    pub stop: StopId,
    /// The thread whose stack holds the frame.
    pub thread: ThreadId,
    /// The frame, numbered as [`DebuggerHandle::backtrace`] presents it.
    pub frame: StackFrameId,
}

/// Inspects one explicit frame of one stopped thread; see
/// [`DebuggerHandle::at`].
///
/// Values in an outer frame come from the registers its callees saved, as
/// for [`DebuggerHandle::select_frame`].
#[derive(Clone, Copy)]
pub struct StopView<'a> {
    handle: &'a DebuggerHandle,
    context: StopContext,
}

impl StopView<'_> {
    /// Returns the frame this view inspects.
    #[must_use]
    pub const fn context(&self) -> StopContext {
        self.context
    }

    /// Reconstructs the thread's stack frames; the view's frame does not
    /// limit them.
    pub async fn backtrace(&self) -> Result<Backtrace> {
        let context = self.context;
        self.handle
            .request(|reply| Request::Backtrace {
                stop_id: context.stop,
                thread_id: context.thread,
                reply,
            })
            .await
    }

    /// Reads the frame's general registers.
    pub async fn registers(&self) -> Result<RegisterSnapshot> {
        let context = self.context;
        self.handle
            .request(|reply| Request::Registers {
                stop_id: context.stop,
                thread_id: context.thread,
                frame: context.frame,
                reply,
            })
            .await
    }

    /// Resolves the frame's location to function and source metadata.
    pub async fn location(&self) -> Result<ExecutionLocation> {
        let context = self.context;
        self.handle
            .request(|reply| Request::StoppedLocation {
                stop_id: context.stop,
                thread_id: context.thread,
                frame: context.frame,
                reply,
            })
            .await
    }

    /// Lazily reads source lines surrounding the frame's location through
    /// the handle's [`SourcePathMap`].
    pub async fn source_context(&self, radius: u32) -> Result<SourceContext> {
        let location = self.location().await?;
        self.handle.source_context_at(location, radius).await
    }

    /// Inspects every visible parameter and local variable of the frame.
    pub async fn variables(&self) -> Result<VariableSnapshot> {
        self.variables_with_limits(InspectionLimits::default())
            .await
    }

    /// Inspects the frame's variables under explicit resource limits.
    pub async fn variables_with_limits(
        &self,
        limits: InspectionLimits,
    ) -> Result<VariableSnapshot> {
        self.variable_query(VariableQuery::All, limits).await
    }

    /// Inspects one exact global as the frame's thread sees it, such as its
    /// instance of a thread-local variable.
    pub async fn global_with_limits(
        &self,
        global: GlobalVariableReference,
        limits: InspectionLimits,
    ) -> Result<Variable> {
        let snapshot = self
            .variable_query(VariableQuery::Global(global), limits)
            .await?;
        snapshot
            .variables
            .first()
            .cloned()
            .ok_or_else(|| Error::VariableNotFound(global.variable.to_string()))
    }

    async fn variable_query(
        &self,
        query: VariableQuery,
        limits: InspectionLimits,
    ) -> Result<VariableSnapshot> {
        let context = self.context;
        self.handle
            .request(|reply| Request::Variables {
                query,
                limits,
                stop_id: context.stop,
                thread_id: context.thread,
                frame: context.frame,
                reply,
            })
            .await
    }

    /// Evaluates an expression in the frame, reading only.
    pub async fn evaluate(&self, expression: &Expression) -> Result<Evaluation> {
        self.evaluate_with(
            expression,
            EvaluationMode::Read,
            InspectionLimits::default(),
        )
        .await
    }

    /// Evaluates an expression in the frame, assigning when `mode` allows,
    /// under explicit resource limits.
    pub async fn evaluate_with(
        &self,
        expression: &Expression,
        mode: EvaluationMode,
        limits: InspectionLimits,
    ) -> Result<Evaluation> {
        let context = self.context;
        let expression = expression.clone();
        self.handle
            .request(|reply| Request::Evaluate {
                expression,
                mode,
                limits,
                stop_id: context.stop,
                thread_id: context.thread,
                frame: context.frame,
                reply,
            })
            .await
    }

    /// The type an expression has in the frame, reading no memory.
    pub async fn expression_type(&self, expression: &Expression) -> Result<TypeInfo> {
        let context = self.context;
        let expression = expression.clone();
        self.handle
            .request(|reply| Request::ExpressionType {
                expression,
                stop_id: context.stop,
                thread_id: context.thread,
                frame: context.frame,
                reply,
            })
            .await
    }

    /// Evaluates an expression in the frame for its value, reading only. A
    /// range has no single value; evaluate it for its page of elements.
    pub async fn inspect(&self, expression: &Expression) -> Result<InspectedValue> {
        self.inspect_with_limits(expression, InspectionLimits::default())
            .await
    }

    /// Evaluates an expression for its value under explicit resource limits.
    pub async fn inspect_with_limits(
        &self,
        expression: &Expression,
        limits: InspectionLimits,
    ) -> Result<InspectedValue> {
        match self
            .evaluate_with(expression, EvaluationMode::Read, limits)
            .await?
        {
            Evaluation::Value { value, .. } => Ok(value),
            _ => Err(Error::InvalidValueRange(
                "a range has no single value; evaluate it for its elements".into(),
            )),
        }
    }

    /// Resolves an expression in the frame to the memory it occupies and
    /// the lifetime of that storage.
    pub async fn resolve_watch_target(&self, expression: &Expression) -> Result<WatchTarget> {
        let context = self.context;
        let expression = expression.clone();
        self.handle
            .request(|reply| Request::ResolveWatchTarget {
                expression,
                stop_id: context.stop,
                thread_id: context.thread,
                frame: context.frame,
                reply,
            })
            .await
    }

    /// Disassembles code as the thread sees it at the stop; see
    /// [`DebuggerHandle::disassemble`].
    pub async fn disassemble(&self, query: DisassemblyQuery) -> Result<Disassembly> {
        let context = self.context;
        self.handle
            .request(|reply| Request::Disassemble {
                query,
                stop_id: context.stop,
                thread_id: context.thread,
                reply,
            })
            .await
    }
}

/// The stop that implicit inspection reads and the thread and frame selected
/// in it.
#[derive(Clone, Copy)]
pub(crate) struct StoppedSelection {
    pub(crate) process: ProcessId,
    pub(crate) stop: StopId,
    pub(crate) thread: ThreadId,
    pub(crate) frame: StackFrameId,
}
