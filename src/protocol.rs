use std::{fmt, path::PathBuf, sync::Arc};

use tokio::sync::oneshot;

use crate::model::numeric_id;

use crate::{
    Backtrace, BreakpointLocation, CodeInstanceId, DereferenceReference, DereferencedValue,
    ExecutionLocation, GlobalVariablePage, GlobalVariableReference, LineNumber, LoadedModule,
    LoadedModuleSnapshot, RegisterSnapshot, Result, ThreadId, ValueChildPage,
    ValueChildrenReference, VariableSnapshot, VirtualAddress,
};

/// Selects data objects to inspect in the stopped thread's selected logical frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariableQuery {
    /// Inspect every visible parameter and local declaration.
    All,
    /// Inspect the innermost visible data object with this name.
    Name(String),
    /// Inspect one exact global catalog entry.
    Global(GlobalVariableReference),
}

/// Selects one bounded page from the immutable global catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalVariableQuery {
    /// Case-sensitive substring matched against source and linkage names.
    pub filter: Option<String>,
    /// Zero-based offset within the deterministic match ordering.
    pub offset: u64,
    /// Maximum entries to return; must be between 1 and 256.
    pub limit: u32,
}

impl Default for GlobalVariableQuery {
    fn default() -> Self {
        Self {
            filter: None,
            offset: 0,
            limit: 100,
        }
    }
}

/// Selects an arbitrary bounded page of one aggregate's children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueChildQuery {
    /// Zero-based offset within the parent's stable child ordering.
    pub offset: u64,
    /// Maximum children to return; must be between 1 and 256.
    pub limit: u32,
}

impl Default for ValueChildQuery {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 32,
        }
    }
}

/// A user-facing request for a logical breakpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakpointSpec {
    /// Break at the uniquely named function.
    Function(String),
    /// Break at every statement address for an exact source line.
    Source { path: PathBuf, line: LineNumber },
    /// Break at every concrete instance of a function declared in one source file.
    FileFunction { path: PathBuf, function: String },
    /// Break at an absolute process virtual address.
    Address(VirtualAddress),
}

impl fmt::Display for BreakpointSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Function(function) => f.write_str(function),
            Self::Source { path, line } => write!(f, "{}:{line}", path.display()),
            Self::FileFunction { path, function } => write!(f, "{}:{function}", path.display()),
            Self::Address(address) => address.fmt(f),
        }
    }
}

numeric_id!(
    BreakpointId,
    "Identifies one logical user breakpoint within a debug session."
);

/// One deduplicated location resolved for a logical breakpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBreakpointLocation {
    /// The address and address space where the trap is installed.
    pub location: BreakpointLocation,
    /// Concrete code instances represented by this location.
    pub code_instances: Arc<[CodeInstanceId]>,
}

/// An immutable logical breakpoint and all locations resolved for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breakpoint {
    /// The breakpoint's session-scoped identifier.
    pub id: BreakpointId,
    /// The user intent that created the breakpoint.
    pub spec: BreakpointSpec,
    /// Every deduplicated location at which the breakpoint is installed.
    pub locations: Arc<[ResolvedBreakpointLocation]>,
}

numeric_id!(
    WatchpointId,
    "Identifies one watchpoint within a debug session."
);

/// The memory accesses that trigger a watchpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WatchAccess {
    /// Stores to any watched byte.
    Write,
    /// Loads from any watched byte, without stores.
    Read,
    /// Loads from or stores to any watched byte.
    ReadWrite,
}

impl fmt::Display for WatchAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Write => "write",
            Self::Read => "read",
            Self::ReadWrite => "read/write",
        })
    }
}

/// What the debugger's hardware watchpoint support can arm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchpointCapabilities {
    /// Hardware slots per thread, shared by every watchpoint. Other users of
    /// the debug hardware, such as perf breakpoints, can leave fewer.
    pub slots: u32,
    /// The widest naturally aligned span one slot covers. Other spans use
    /// several slots.
    pub max_slot_bytes: u64,
    /// The access kinds the hardware can report exactly.
    pub access: Arc<[WatchAccess]>,
}

/// The storage lifetime that bounds a watched object.
///
/// A scoped watchpoint is invalidated once its storage may belong to a
/// different object, instead of reporting accesses to unrelated data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchScope {
    /// An explicit address, or storage reached through a pointer. The
    /// debugger cannot know when it is reused and never invalidates it.
    Location,
    /// Static storage owned by a loaded module.
    Static {
        /// The module whose unload invalidates the watchpoint.
        module: crate::ModuleId,
    },
    /// One thread's instance of thread-local storage.
    ThreadLocal {
        /// The thread whose exit invalidates the watchpoint.
        thread: ThreadId,
    },
    /// A local variable or parameter of one function activation.
    Frame {
        /// The thread executing the activation.
        thread: ThreadId,
        /// The activation's canonical frame address.
        activation: VirtualAddress,
    },
}

/// Debugger-internal evidence used to decide whether a frame-scoped object
/// is still live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameScopeEvidence {
    pub module: crate::ModuleId,
    pub image: crate::ModuleImageId,
    pub function: CodeInstanceId,
    pub ranges: Arc<[crate::AddressRange<crate::ImageAddress>]>,
}

/// An opaque capability for one watchable memory object resolved at one
/// stopped snapshot.
///
/// The address is fixed when the target is resolved, so watching an
/// expression that passes through a pointer keeps watching the original
/// pointee after the pointer changes. The target can only be armed at the
/// stop that resolved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchTarget {
    pub(crate) stop_id: StopId,
    pub(crate) expression: crate::ValueExpression,
    pub(crate) address: VirtualAddress,
    pub(crate) byte_size: u64,
    pub(crate) type_info: Option<crate::TypeInfo>,
    pub(crate) scope: WatchScope,
    pub(crate) frame: Option<FrameScopeEvidence>,
}

impl WatchTarget {
    /// Returns the stopped snapshot that resolved this target.
    #[must_use]
    pub const fn stop_id(&self) -> StopId {
        self.stop_id
    }

    /// Returns the expression that named the object.
    #[must_use]
    pub const fn expression(&self) -> &crate::ValueExpression {
        &self.expression
    }

    /// Returns the first watched byte.
    #[must_use]
    pub const fn address(&self) -> VirtualAddress {
        self.address
    }

    /// Returns the number of watched bytes.
    #[must_use]
    pub const fn byte_size(&self) -> u64 {
        self.byte_size
    }

    /// Returns the object's resolved type.
    #[must_use]
    pub const fn type_info(&self) -> Option<&crate::TypeInfo> {
        self.type_info.as_ref()
    }

    /// Returns the lifetime that bounds the object's storage.
    #[must_use]
    pub const fn scope(&self) -> &WatchScope {
        &self.scope
    }
}

/// What a new watchpoint observes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchpointSpec {
    /// An object resolved at the current stop.
    Target(Box<WatchTarget>),
    /// Explicit bytes of the process address space.
    Location {
        /// The first watched byte.
        address: VirtualAddress,
        /// The number of watched bytes.
        byte_size: u64,
    },
}

/// An armed hardware watchpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watchpoint {
    /// The watchpoint's session-scoped identifier.
    pub id: WatchpointId,
    /// The accesses it reports.
    pub access: WatchAccess,
    /// The expression that named the object, absent for explicit locations.
    pub expression: Option<crate::ValueExpression>,
    /// The first watched byte.
    pub address: VirtualAddress,
    /// The number of watched bytes.
    pub byte_size: u64,
    /// The watched object's type, when it was resolved from an expression.
    pub type_info: Option<crate::TypeInfo>,
    /// The lifetime that bounds the watched storage.
    pub scope: WatchScope,
    /// The naturally aligned hardware spans that exactly cover the bytes.
    pub coverage: Arc<[crate::AddressRange<VirtualAddress>]>,
}

/// One watchpoint reported by one thread's access.
///
/// Hardware reports that an access happened, not what it changed: a store of
/// an identical value is reported with equal bytes. Accesses made by the
/// kernel on the process's behalf, such as `read(2)` filling a watched
/// buffer, are never reported, so `previous` is the value last observed by
/// the debugger rather than necessarily the value immediately before this
/// access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchpointHit {
    /// The watchpoint that reported the access.
    pub watchpoint: WatchpointId,
    /// The thread whose instruction made the access.
    pub thread: ThreadId,
    /// The watched bytes last observed by the debugger, when readable.
    pub previous: Option<Arc<[u8]>>,
    /// The watched bytes once every thread stopped, when readable.
    pub current: Option<Arc<[u8]>>,
}

impl WatchpointHit {
    /// Whether the watched bytes differ from the last observed bytes.
    #[must_use]
    pub fn changed(&self) -> bool {
        self.previous != self.current
    }
}

/// Why a scoped watchpoint stopped watching its storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchpointInvalidation {
    /// The function activation or lexical block owning the object ended.
    ScopeExited,
    /// The thread owning the object exited.
    OwnerThreadExited,
    /// The module owning the object was unloaded.
    ModuleUnloaded,
}

/// A watchpoint that was removed because its storage's lifetime ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidatedWatchpoint {
    /// The removed watchpoint.
    pub watchpoint: Watchpoint,
    /// Why its storage is no longer watched.
    pub reason: WatchpointInvalidation,
}

/// The logical frame selected for presentation at a stopped instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresentedFrame {
    /// The physical containing frame is selected.
    Physical,
    /// One concrete inline instance is selected.
    Inline(CodeInstanceId),
    /// The debug metadata does not identify one compatible inline chain.
    Ambiguous(Arc<[CodeInstanceId]>),
}

/// Controller-owned logical presentation for the selected stopped thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FramePresentation {
    /// The machine instruction to which this presentation is tied.
    pub instruction: VirtualAddress,
    /// The logical frame currently selected at that instruction.
    pub frame: PresentedFrame,
    /// Active inline frames intentionally hidden below the selection.
    pub hidden_inline_frames: u32,
}

numeric_id!(
    ProcessId,
    "Identifies an inferior process; local attach accepts an operating-system process ID."
);

/// Selects a post-mortem core dump and how its module files are trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreDumpOptions {
    /// The ELF core file to open.
    pub core: PathBuf,
    /// The executable that produced the dump. By default the path recorded in
    /// the dump is used.
    pub executable: Option<PathBuf>,
    /// Use module files that the dump cannot prove match its recorded images.
    ///
    /// Such modules contribute debug metadata only: their file contents never
    /// substitute for memory the dump did not save. A file that cannot be
    /// placed at its recorded image is still refused, since relocating it
    /// would be a guess.
    pub allow_module_mismatch: bool,
}

impl CoreDumpOptions {
    /// Opens `core` with its recorded executable and strict module identity.
    pub fn new(core: impl Into<PathBuf>) -> Self {
        Self {
            core: core.into(),
            executable: None,
            allow_module_mismatch: false,
        }
    }
}

/// How a module file was matched to an image recorded in a core dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleIdentity {
    /// The dumped GNU build-id note equals the file's.
    BuildId,
    /// Every saved byte of the file's read-only segments equals the file.
    SavedContent {
        /// The number of saved bytes compared.
        compared_bytes: u64,
    },
    /// The file differs from the dumped image; it was loaded only because
    /// module mismatches were explicitly allowed.
    Mismatched {
        /// Why the file is known to differ.
        detail: Arc<str>,
    },
    /// The dump saved nothing that could confirm the file; it was loaded only
    /// because module mismatches were explicitly allowed.
    Unverified,
}

impl ModuleIdentity {
    /// Whether the file is proven to be the dumped image.
    #[must_use]
    pub const fn is_verified(&self) -> bool {
        matches!(self, Self::BuildId | Self::SavedContent { .. })
    }
}

/// The debugger's use of one image recorded in a core dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreModuleState {
    /// A file was loaded for the image.
    Loaded {
        /// The loaded module and the file providing its metadata.
        module: crate::LoadedModuleRecord,
        /// The evidence that the file is the recorded image.
        identity: ModuleIdentity,
    },
    /// No file exists at the recorded path; the image's frames and memory
    /// outside the dump stay unavailable.
    Missing,
}

/// One executable or shared-library image recorded in a core dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreModule {
    /// The image path recorded by the dump.
    pub recorded_path: Arc<PathBuf>,
    /// The image's lowest mapped address at dump time.
    pub start: VirtualAddress,
    /// Whether and how a file was loaded for the image.
    pub state: CoreModuleState,
}

/// Immutable description of an opened post-mortem core dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreDumpInfo {
    /// The core file.
    pub path: Arc<PathBuf>,
    /// The process that produced the dump.
    pub process_id: ProcessId,
    /// The process name recorded by the dump.
    pub process_name: Arc<str>,
    /// The leading command-line arguments recorded by the dump.
    pub arguments: Arc<str>,
    /// The signal that terminated the process, when recorded.
    pub exception: Option<ExceptionInfo>,
    /// Recorded images, beginning with the main executable.
    pub modules: Arc<[CoreModule]>,
}

numeric_id!(
    StopId,
    "Identifies an externally observable stopped snapshot."
);

numeric_id!(
    ExecutionId,
    "Identifies one accepted execution-control operation."
);

/// Selects which execution contexts a control operation resumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeScope {
    /// Resume every eligible thread in one process.
    Process(ProcessId),
    /// Resume only one thread.
    Thread(ThreadId),
}

/// Selects the behavior of a stepping operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    /// Execute one machine instruction.
    Instruction,
    /// Advance to a different source location, entering calls.
    IntoSource,
    /// Advance to a different source location without stopping in callees.
    OverSource,
    /// Run until the selected frame returns.
    Out,
}

/// Selects what happens to an exception pending on a stopped thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExceptionDisposition {
    /// Preserve the target's original exception delivery.
    Pass,
    /// Discard the pending exception.
    Suppress,
}

/// Platform-neutral information about an exception that stopped an inferior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExceptionInfo {
    /// The platform-defined exception code.
    pub code: u64,
    /// A human-readable description of the exception.
    pub description: Arc<str>,
}

impl ExceptionInfo {
    /// Creates exception information from a platform code and description.
    pub fn new(code: u64, description: impl Into<Arc<str>>) -> Self {
        Self {
            code,
            description: description.into(),
        }
    }
}

/// Describes how an inferior process exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitStatus {
    /// The process returned an exit code.
    Code(i64),
    /// The process was terminated by an exception.
    Terminated(ExceptionInfo),
}

/// Describes why execution stopped or completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The debugger established an initial coherent stop after attaching.
    Attach,
    /// Execution reached an installed breakpoint.
    Breakpoint { address: VirtualAddress },
    /// A thread accessed watched memory. The thread stops after the
    /// accessing instruction, or within a repeated string instruction that
    /// has more iterations left.
    Watchpoint {
        /// Every watchpoint this thread's access reported.
        hits: Arc<[WatchpointHit]>,
    },
    /// Watched memory was accessed after the watched object's lifetime ended,
    /// so the affected watchpoints were removed instead of reporting
    /// accesses to unrelated data.
    WatchpointInvalidated {
        /// The removed watchpoints.
        invalidated: Arc<[InvalidatedWatchpoint]>,
    },
    /// A new thread could not be armed with the process's watchpoints. It
    /// was stopped before running so it never executes unwatched.
    WatchpointArmFailed {
        /// The unarmed thread.
        thread_id: ThreadId,
        /// Why arming failed.
        description: Arc<str>,
    },
    /// A stepping operation completed.
    Step { kind: StepKind },
    /// Execution stopped at the user's request.
    Pause,
    /// Execution stopped because of an exception.
    Exception(ExceptionInfo),
    /// The process replaced its executable image.
    Exec,
    /// A thread-specific execution operation ended because its thread exited.
    ThreadExited {
        /// The thread that exited.
        thread_id: ThreadId,
        /// How that thread exited.
        status: ExitStatus,
    },
    /// The backend could not safely classify a native stop.
    Unclassifiable {
        /// Diagnostic details retained by the platform edge.
        description: Arc<str>,
    },
    /// The inferior exited.
    Exited(ExitStatus),
    /// A post-mortem core dump was opened; execution can never resume.
    CoreDump {
        /// The signal that terminated the process, when the dump recorded one.
        exception: Option<ExceptionInfo>,
    },
}

/// The externally observable execution state of one live thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadState {
    /// The thread is running or starting.
    Running,
    /// The thread is stopped and safe to inspect.
    Stopped {
        /// The thread's own stop reason, when it produced an interesting event.
        reason: Option<StopReason>,
    },
}

/// An immutable view of one thread at a debugger revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSnapshot {
    /// The thread being described.
    pub id: ThreadId,
    /// The thread's observable execution state.
    pub state: ThreadState,
}

/// The externally observable state of the inferior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InferiorState {
    /// No inferior process exists.
    NotRunning,
    /// The inferior is starting or running.
    Running {
        /// The running process.
        process_id: ProcessId,
        /// The operation currently controlling execution, when known.
        execution_id: Option<ExecutionId>,
    },
    /// The inferior is stopped and may be inspected.
    Stopped {
        /// The stopped process.
        process_id: ProcessId,
        /// The immutable stopped snapshot identifier.
        stop_id: StopId,
        /// The thread selected by the stop.
        thread_id: ThreadId,
        /// The reason execution stopped.
        reason: StopReason,
    },
}

/// An immutable view of debugger state at one revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateSnapshot {
    /// The debugger revision represented by this snapshot.
    pub revision: u64,
    /// The current inferior state.
    pub inferior: InferiorState,
    /// The current stopped snapshot, when the inferior is stopped.
    pub stop_id: Option<StopId>,
    /// The thread selected for implicit inspection commands.
    pub selected_thread: Option<ThreadId>,
    /// All live threads known at this revision.
    pub threads: Arc<[ThreadSnapshot]>,
    /// Logical presentation for the selected thread at this stop.
    pub presentation: Option<FramePresentation>,
    /// The logical breakpoints requested by clients.
    pub breakpoints: Arc<[Breakpoint]>,
    /// The watchpoints armed in the current process.
    pub watchpoints: Arc<[Watchpoint]>,
}

/// A state or lifecycle event emitted by the debugger.
///
/// Every event carries the state revision it produced; a
/// [`StateSnapshot`] at that revision or later reflects it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebuggerEvent {
    /// Debugger state changed in a way no more specific event describes.
    StateChanged { revision: u64 },
    /// A launched inferior began executing under `execution_id`.
    InferiorLaunched {
        revision: u64,
        process_id: ProcessId,
        execution_id: ExecutionId,
    },
    /// An existing process was attached and coherently stopped.
    InferiorAttached {
        revision: u64,
        process_id: ProcessId,
    },
    /// Stopped threads resumed under `execution_id`.
    InferiorContinued {
        revision: u64,
        process_id: ProcessId,
        execution_id: ExecutionId,
        resumed: ResumeScope,
    },
    /// Every live thread is stopped and safe to inspect.
    InferiorStopped {
        revision: u64,
        process_id: ProcessId,
        /// The execution that ended, or `None` for a stop no client caused.
        execution_id: Option<ExecutionId>,
        stop_id: StopId,
        /// The thread whose event caused the stop.
        thread_id: ThreadId,
        reason: StopReason,
    },
    /// A new thread appeared in the inferior.
    ThreadStarted {
        revision: u64,
        process_id: ProcessId,
        thread_id: ThreadId,
    },
    /// One thread of a still-running inferior exited.
    ThreadExited {
        revision: u64,
        process_id: ProcessId,
        thread_id: ThreadId,
        status: ExitStatus,
    },
    /// A shared object was mapped into the inferior.
    ModuleLoaded {
        revision: u64,
        module: crate::LoadedModuleRecord,
    },
    /// A shared object was unmapped from the inferior.
    ModuleUnloaded {
        revision: u64,
        module: crate::LoadedModuleRecord,
    },
    /// The inferior process exited.
    InferiorExited {
        revision: u64,
        process_id: ProcessId,
        /// The execution that ended, when one was active.
        execution_id: Option<ExecutionId>,
        status: ExitStatus,
    },
    /// The debugger released an attached process, which keeps running.
    InferiorDetached {
        revision: u64,
        process_id: ProcessId,
    },
    /// The set of logical breakpoints changed.
    BreakpointsChanged { revision: u64 },
    /// The set of armed watchpoints changed.
    WatchpointsChanged { revision: u64 },
    /// Scoped watchpoints were removed because their storage's lifetime
    /// ended. A following `WatchpointsChanged` publishes the new set.
    WatchpointsInvalidated {
        revision: u64,
        invalidated: Arc<[InvalidatedWatchpoint]>,
    },
}

impl DebuggerEvent {
    /// Returns the state revision this event produced.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        match self {
            Self::StateChanged { revision }
            | Self::InferiorLaunched { revision, .. }
            | Self::InferiorAttached { revision, .. }
            | Self::InferiorContinued { revision, .. }
            | Self::InferiorStopped { revision, .. }
            | Self::ThreadStarted { revision, .. }
            | Self::ThreadExited { revision, .. }
            | Self::ModuleLoaded { revision, .. }
            | Self::ModuleUnloaded { revision, .. }
            | Self::InferiorExited { revision, .. }
            | Self::InferiorDetached { revision, .. }
            | Self::BreakpointsChanged { revision }
            | Self::WatchpointsChanged { revision }
            | Self::WatchpointsInvalidated { revision, .. } => *revision,
        }
    }
}

/// Carries one request's result back to the client awaiting it.
pub type Reply<T> = oneshot::Sender<Result<T>>;

/// A client request to the controller; each carries its reply channel.
pub enum Request {
    AddBreakpoint {
        spec: BreakpointSpec,
        reply: Reply<Breakpoint>,
    },
    RemoveBreakpoint {
        id: BreakpointId,
        reply: Reply<Breakpoint>,
    },
    RemoveAllBreakpoints {
        reply: Reply<Arc<[Breakpoint]>>,
    },
    ResolveWatchTarget {
        expression: crate::ValueExpression,
        stop_id: StopId,
        thread_id: ThreadId,
        reply: Reply<WatchTarget>,
    },
    AddWatchpoint {
        spec: WatchpointSpec,
        access: WatchAccess,
        reply: Reply<Watchpoint>,
    },
    RemoveWatchpoint {
        id: WatchpointId,
        reply: Reply<Watchpoint>,
    },
    RemoveAllWatchpoints {
        reply: Reply<Arc<[Watchpoint]>>,
    },
    Launch {
        reply: Reply<ExecutionId>,
    },
    Attach {
        process_id: ProcessId,
        reply: Reply<StopId>,
    },
    Continue {
        process_id: ProcessId,
        stop_id: StopId,
        scope: ResumeScope,
        exception: ExceptionDisposition,
        reply: Reply<ExecutionId>,
    },
    Step {
        process_id: ProcessId,
        stop_id: StopId,
        thread_id: ThreadId,
        kind: StepKind,
        exception: ExceptionDisposition,
        reply: Reply<ExecutionId>,
    },
    Pause {
        process_id: ProcessId,
        reply: Reply<ExecutionId>,
    },
    ReadMemory {
        process_id: ProcessId,
        stop_id: StopId,
        address: VirtualAddress,
        byte_count: u64,
        reply: Reply<crate::MemoryRead>,
    },
    ReadWord {
        process_id: ProcessId,
        stop_id: StopId,
        address: VirtualAddress,
        reply: Reply<u64>,
    },
    WriteWord {
        process_id: ProcessId,
        stop_id: StopId,
        address: VirtualAddress,
        value: u64,
        reply: Reply<()>,
    },
    LoadedModule {
        reply: Reply<LoadedModule>,
    },
    LoadedModules {
        reply: Reply<LoadedModuleSnapshot>,
    },
    ModuleImage {
        module: crate::ModuleId,
        reply: Reply<Arc<crate::ModuleImage>>,
    },
    StoppedLocation {
        stop_id: StopId,
        thread_id: ThreadId,
        reply: Reply<ExecutionLocation>,
    },
    Snapshot {
        reply: Reply<StateSnapshot>,
    },
    Backtrace {
        stop_id: StopId,
        thread_id: ThreadId,
        reply: Reply<Backtrace>,
    },
    Registers {
        stop_id: StopId,
        thread_id: ThreadId,
        reply: Reply<RegisterSnapshot>,
    },
    Variables {
        query: VariableQuery,
        limits: crate::InspectionLimits,
        stop_id: StopId,
        thread_id: ThreadId,
        reply: Reply<VariableSnapshot>,
    },
    Inspect {
        expression: crate::ValueExpression,
        limits: crate::InspectionLimits,
        stop_id: StopId,
        thread_id: ThreadId,
        reply: Reply<crate::InspectedValue>,
    },
    InspectRange {
        expression: crate::ValueExpression,
        range: crate::ValueIndexRange,
        limits: crate::InspectionLimits,
        stop_id: StopId,
        thread_id: ThreadId,
        reply: Reply<ValueChildPage>,
    },
    Dereference {
        reference: DereferenceReference,
        limits: crate::InspectionLimits,
        reply: Reply<DereferencedValue>,
    },
    ValueChildren {
        reference: Arc<ValueChildrenReference>,
        query: ValueChildQuery,
        limits: crate::InspectionLimits,
        reply: Reply<ValueChildPage>,
    },
    Globals {
        query: GlobalVariableQuery,
        reply: Reply<GlobalVariablePage>,
    },
    SelectThread {
        stop_id: StopId,
        thread_id: ThreadId,
        reply: Reply<()>,
    },
    Shutdown {
        reply: Reply<()>,
    },
}
