use std::error::Error as StdError;
use std::path::PathBuf;
use std::sync::Arc;

/// Every failure a debugger request can report.
///
/// A message either embeds its cause or exposes it as [`StdError::source`],
/// never both, so error reports that walk the chain print each cause once.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("debug information error: {0}")]
    DebugInfo(Box<dyn StdError + Send + Sync>),
    #[error("debugger backend error: {0}")]
    Backend(Box<dyn StdError + Send + Sync>),

    #[error("no function named '{0}' was found")]
    FunctionNotFound(String),
    #[error("multiple functions named '{0}' were found")]
    DuplicateFunction(String),
    #[error("no source file matching '{0}' was found")]
    SourceFileNotFound(PathBuf),
    #[error("source path '{path}' is ambiguous; matches: {matches:?}")]
    AmbiguousSourceFile {
        path: PathBuf,
        matches: Vec<PathBuf>,
    },
    #[error("source line {line} in {path} has no code at or after it in its function")]
    SourceLineUnavailable { path: PathBuf, line: u64 },
    #[error("breakpoint {0} was not found")]
    BreakpointNotFound(u64),
    #[error("invalid hit condition: {0}")]
    InvalidHitCondition(String),
    #[error("invalid condition: {0}")]
    InvalidCondition(String),
    #[error("invalid log message: {0}")]
    InvalidLogMessage(String),
    #[error("no symbol named '{0}' was found")]
    SymbolNotFound(String),
    #[error("multiple symbols named '{0}' were found")]
    DuplicateSymbol(String),
    #[error("no visible variable or parameter named '{0}' was found")]
    VariableNotFound(String),
    #[error("multiple equally visible variables or parameters named '{0}' were found")]
    AmbiguousVariable(String),
    #[error("invalid value expression: {0}")]
    InvalidValueExpression(String),
    /// An expression that has no value, and the part of its text at fault.
    #[error("{0}")]
    Expression(crate::ExpressionError),
    #[error("record type '{type_name}' has no member named '{member}'")]
    MemberNotFound { member: String, type_name: Arc<str> },
    #[error("member '{member}' is ambiguous in record type '{type_name}'")]
    AmbiguousMember { member: String, type_name: Arc<str> },
    #[error("'{base}' is not a base class of '{type_name}'")]
    BaseNotFound { base: Arc<str>, type_name: Arc<str> },
    #[error("'{type_name}' has several '{base}' base class subobjects")]
    AmbiguousBase { base: Arc<str>, type_name: Arc<str> },
    #[error("cannot select member '{member}' from non-record type '{type_name}'")]
    MemberAccessOnNonRecord { member: String, type_name: Arc<str> },
    #[error("cannot index non-array or non-slice type '{type_name}'")]
    IndexAccessOnNonIndexable { type_name: Arc<str> },
    #[error(
        "index {index} is outside the source bounds starting at {lower_bound} with {count} elements"
    )]
    ValueIndexOutOfBounds {
        index: i128,
        lower_bound: i128,
        count: u64,
    },
    #[error(
        "array type '{type_name}' requires {expected} indices at this level, but {supplied} were supplied"
    )]
    IncompleteArrayIndex {
        type_name: Arc<str>,
        expected: usize,
        supplied: usize,
    },
    #[error("invalid value range: {0}")]
    InvalidValueRange(Arc<str>),
    #[error("global variable selector '{selector}' is ambiguous: {candidates:?}")]
    AmbiguousGlobalVariable {
        selector: String,
        candidates: Vec<crate::GlobalVariableCandidate>,
    },
    #[error("variable inspection is unavailable for the selected logical frame")]
    VariableContextUnsupported,
    #[error("global catalog page limit {0} is outside 1..=256")]
    InvalidGlobalPageLimit(u32),
    #[error("value child page limit {0} is outside 1..=256")]
    InvalidValueChildPageLimit(u32),
    #[error("{resource:?} inspection limit {value} is outside 1..={maximum}")]
    InvalidInspectionLimit {
        resource: crate::InspectionLimit,
        value: u64,
        maximum: u64,
    },
    #[error("loaded global selector '{selector}' is ambiguous: {candidates:?}")]
    AmbiguousLoadedGlobalVariable {
        selector: String,
        candidates: Vec<crate::GlobalVariableReference>,
    },
    #[error("loaded module {0} is unavailable")]
    ModuleNotLoaded(crate::ModuleId),
    #[error("loaded module identity refers to a stale image")]
    StaleModuleImage,
    #[error("variable runtime access failed: {0}")]
    VariableRuntime(Arc<str>),

    #[error("the inferior is already running")]
    AlreadyRunning,
    #[error("process identifier {0} is invalid")]
    InvalidProcessId(u64),
    #[error(
        "process {0} has several threads; only a single-threaded process can be launched through"
    )]
    ProcessHasThreads(u64),
    #[error("could not determine a stable identity for process {0}")]
    ProcessIdentityUnavailable(u64),
    #[error("the target process changed while the debugger was attaching")]
    TargetChangedDuringAttach,
    #[error("process {0} is no longer the process that was held")]
    HeldProcessGone(u64),
    #[error("the inferior has not been launched")]
    NotRunning,
    #[error("the inferior is not stopped")]
    NotStopped,
    #[error("the inferior is already stopped")]
    AlreadyStopped,
    #[error("{0} is not a signal of this target")]
    UnknownSignal(u64),
    #[error("thread {0} is not a thread of the inferior")]
    UnknownThread(crate::ThreadId),
    #[error("the requested stopped snapshot is no longer current")]
    StaleStop,
    #[error("an unclassifiable native stop cannot be resumed safely")]
    UnclassifiableStop,
    #[error("address arithmetic overflow")]
    AddressOverflow,
    #[error("memory read of {requested} bytes exceeds the {maximum}-byte limit")]
    MemoryReadTooLarge { requested: u64, maximum: u64 },
    #[error("cannot write {requested} bytes at once; at most {maximum} can be written")]
    MemoryWriteTooLarge { requested: u64, maximum: u64 },
    #[error("memory at {0} cannot be read")]
    MemoryNotReadable(crate::VirtualAddress),
    #[error("memory at {0} cannot be written")]
    MemoryNotWritable(crate::VirtualAddress),
    #[error("address is outside the loaded module")]
    AddressOutsideModule,
    #[error("the stopped location is unavailable")]
    LocationUnavailable,
    #[error("the active inline frame is ambiguous")]
    AmbiguousInlineFrame,
    #[error("frame {frame} does not exist; the backtrace has {frames} frames")]
    FrameNotFound {
        frame: crate::StackFrameId,
        frames: u32,
    },
    #[error("cannot step from the selected frame: {0}")]
    FrameStepUnsupported(Arc<str>),
    #[error("a step of thread {stepping} cannot resume thread {resumed} alone")]
    StepScopeMismatch {
        stepping: crate::ThreadId,
        resumed: crate::ThreadId,
    },
    #[error("no source location is available for the stopped instruction")]
    SourceLocationUnavailable,
    #[error("failed to read source file {path}: {error}")]
    SourceFileRead {
        path: PathBuf,
        error: std::io::Error,
    },
    #[error("source file {path} does not exist{}", missing_source_detail(.tried))]
    SourceFileMissing { path: PathBuf, tried: Vec<PathBuf> },
    #[error("a source path rule needs a nonempty prefix to replace")]
    EmptySourcePathPrefix,
    #[error("source line {line} is outside {path}")]
    SourceLineOutOfRange { path: PathBuf, line: u64 },

    #[error("invalid core dump: {0}")]
    InvalidCoreDump(String),
    #[error("could not determine the core dump's executable: {0}")]
    CoreExecutableUnavailable(String),
    #[error(
        "{path} does not match the image recorded in the core dump: {detail}; allow module mismatches to use it anyway"
    )]
    CoreModuleMismatch { path: PathBuf, detail: String },
    #[error(
        "the core dump saved nothing that verifies {path}; allow module mismatches to use it anyway"
    )]
    CoreModuleUnverified { path: PathBuf },
    #[error("{path} cannot be placed at the image recorded in the core dump: {detail}")]
    CoreModuleUnplaceable { path: PathBuf, detail: String },
    #[error("cannot search {path} for core dump modules: {error}")]
    CoreModuleSearch {
        path: PathBuf,
        error: std::io::Error,
    },
    #[error("failed to read core dump module {path}: {error}")]
    CoreModuleRead {
        path: PathBuf,
        error: std::io::Error,
    },
    #[error("a post-mortem core dump cannot execute, be modified, or hold breakpoints")]
    PostMortemTarget,

    #[error("watchpoint {0} was not found")]
    WatchpointNotFound(u64),
    #[error("{0} watchpoints are unsupported by this target's debug hardware")]
    UnsupportedWatchAccess(crate::WatchAccess),
    #[error("cannot watch {byte_size} bytes at {address}: {reason}")]
    InvalidWatchRange {
        address: crate::VirtualAddress,
        byte_size: u64,
        reason: Arc<str>,
    },
    #[error(
        "the watchpoint needs {required} hardware slots but only {available} remain; remove a watchpoint or watch fewer bytes"
    )]
    WatchpointCapacity { required: u64, available: u64 },
    #[error(
        "thread {thread} has no free debug registers; another hardware-breakpoint user such as perf holds them"
    )]
    WatchpointHardwareBusy { thread: crate::ThreadId },
    #[error("hardware watchpoints are unavailable on this target: {0}")]
    HardwareWatchpointsUnavailable(Arc<str>),
    #[error("cannot watch a value that is not stored in memory: {0}")]
    WatchTargetNotInMemory(Arc<str>),
    #[error("cannot watch an unavailable value: {0}")]
    WatchTargetUnavailable(Arc<str>),
    #[error("cannot watch this value: {0}")]
    WatchTargetUnsupported(Arc<str>),

    #[error("disassembly is unsupported for {0:?} targets")]
    DisassemblyUnsupported(crate::Architecture),
    #[error(
        "a disassembly window needs at most {max_before} instructions before its address, at most {max_after} from it, and at least one in all; {before} and {after} were requested",
        max_before = crate::disassembly::MAX_WINDOW_BEFORE,
        max_after = crate::disassembly::MAX_WINDOW_AFTER
    )]
    InvalidDisassemblyWindow { before: u32, after: u32 },
    #[error("no function or code symbol contains {0}")]
    NoFunctionContainsAddress(crate::VirtualAddress),

    #[error("debugger backend thread panicked")]
    BackendThreadPanicked,
    #[error("debugger request was cancelled")]
    RequestCancelled,
    #[error("the view failed: {0}")]
    ViewFailed(Arc<str>),
    /// Inspection stopped for run control waiting behind it; the controller
    /// serves the request again after the run control, and no client sees
    /// this error.
    #[error("inspection was interrupted by run control")]
    Interrupted,
    #[error("debugger request queue is closed")]
    RequestQueueClosed,
    #[error("debugger event subscriber fell behind by {0} events")]
    EventStreamLagged(u64),
    #[error("debugger shutdown timed out")]
    ShutdownTimedOut,
}

/// The result of a debugger request.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn debug_info(error: impl StdError + Send + Sync + 'static) -> Self {
        Self::DebugInfo(Box::new(error))
    }

    pub(crate) fn backend(error: impl StdError + Send + Sync + 'static) -> Self {
        Self::Backend(Box::new(error))
    }
}

/// Names the mapped locations tried for a missing source file, which follow
/// the recorded path itself.
fn missing_source_detail(tried: &[PathBuf]) -> String {
    let mapped = &tried[..tried.len().saturating_sub(1)];
    if mapped.is_empty() {
        return String::new();
    }
    let paths = mapped
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    format!(", nor do its mapped paths {}", paths.join(", "))
}
