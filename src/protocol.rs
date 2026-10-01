use std::{fmt, path::PathBuf, sync::Arc};

use tokio::sync::oneshot;

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

/// Identifies one logical user breakpoint within a debug session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BreakpointId(u64);

impl BreakpointId {
    /// Creates a breakpoint identifier from its numeric representation.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric representation of this identifier.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for BreakpointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

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

/// Identifies an inferior process; local attach accepts an operating-system process ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProcessId(u64);

impl ProcessId {
    /// Creates a process identifier from its numeric representation.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric process identifier.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ProcessId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

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

/// Identifies an externally observable stopped snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StopId(u64);

impl StopId {
    /// Creates a stop identifier from its numeric representation.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric representation of this identifier.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for StopId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Identifies one accepted execution-control operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExecutionId(u64);

impl ExecutionId {
    /// Creates an execution identifier from its numeric representation.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric representation of this identifier.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ExecutionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

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
        /// Whether every live thread is safe to inspect.
        all_threads_stopped: bool,
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
}

/// A state or lifecycle event emitted by the debugger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebuggerEvent {
    StateChanged {
        revision: u64,
    },
    InferiorLaunched {
        revision: u64,
        process_id: ProcessId,
        execution_id: ExecutionId,
    },
    InferiorAttached {
        /// The state revision containing the attached inferior.
        revision: u64,
        /// The attached process.
        process_id: ProcessId,
    },
    InferiorContinued {
        revision: u64,
        process_id: ProcessId,
        execution_id: ExecutionId,
        resumed: ResumeScope,
    },
    InferiorStopped {
        revision: u64,
        process_id: ProcessId,
        execution_id: Option<ExecutionId>,
        stop_id: StopId,
        thread_id: ThreadId,
        all_threads_stopped: bool,
        reason: StopReason,
    },
    ThreadStarted {
        revision: u64,
        process_id: ProcessId,
        thread_id: ThreadId,
    },
    ThreadExited {
        revision: u64,
        process_id: ProcessId,
        thread_id: ThreadId,
        status: ExitStatus,
    },
    ModuleLoaded {
        revision: u64,
        module: crate::LoadedModuleRecord,
    },
    ModuleUnloaded {
        revision: u64,
        module: crate::LoadedModuleRecord,
    },
    InferiorExited {
        revision: u64,
        process_id: ProcessId,
        execution_id: Option<ExecutionId>,
        status: ExitStatus,
    },
    InferiorDetached {
        /// The state revision after detaching.
        revision: u64,
        /// The process released from debugger control.
        process_id: ProcessId,
    },
    BreakpointsChanged {
        revision: u64,
    },
}

pub type Reply<T> = oneshot::Sender<Result<T>>;

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
