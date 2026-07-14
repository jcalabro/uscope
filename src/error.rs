use std::error::Error as StdError;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("debug information error: {0}")]
    DebugInfo(#[source] Box<dyn StdError + Send + Sync>),
    #[error("debugger backend error: {0}")]
    Backend(#[source] Box<dyn StdError + Send + Sync>),

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
    #[error("source line {line} in {path} has no breakpoint location")]
    SourceLineUnavailable { path: PathBuf, line: u64 },
    #[error("breakpoint {0} was not found")]
    BreakpointNotFound(u64),
    #[error("no symbol named '{0}' was found")]
    SymbolNotFound(String),
    #[error("multiple symbols named '{0}' were found")]
    DuplicateSymbol(String),
    #[error("no visible variable or parameter named '{0}' was found")]
    VariableNotFound(String),
    #[error("multiple equally visible variables or parameters named '{0}' were found")]
    AmbiguousVariable(String),
    #[error("global variable selector '{selector}' is ambiguous: {candidates:?}")]
    AmbiguousGlobalVariable {
        selector: String,
        candidates: Vec<crate::GlobalVariableCandidate>,
    },
    #[error("variable inspection is unavailable for the selected logical frame")]
    VariableContextUnsupported,
    #[error("global catalog page limit {0} is outside 1..=256")]
    InvalidGlobalPageLimit(u32),
    #[error("loaded global selector '{selector}' is ambiguous: {candidates:?}")]
    AmbiguousLoadedGlobalVariable {
        selector: String,
        candidates: Vec<crate::GlobalVariableReference>,
    },
    #[error("loaded module {0} is unavailable")]
    ModuleNotLoaded(crate::ModuleId),
    #[error("loaded module identity refers to a stale image")]
    StaleModuleImage,

    #[error("the inferior is already running")]
    AlreadyRunning,
    #[error("the inferior has not been launched")]
    NotRunning,
    #[error("the inferior is not stopped")]
    NotStopped,
    #[error("the requested stopped snapshot is no longer current")]
    StaleStop,
    #[error("an unclassifiable native stop cannot be resumed safely")]
    UnclassifiableStop,
    #[error("address arithmetic overflow")]
    AddressOverflow,
    #[error("address is outside the loaded module")]
    AddressOutsideModule,
    #[error("the stopped location is unavailable")]
    LocationUnavailable,
    #[error("the active inline frame is ambiguous")]
    AmbiguousInlineFrame,
    #[error("no source location is available for the stopped instruction")]
    SourceLocationUnavailable,
    #[error("failed to read source file {path}: {source}")]
    SourceFileRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("source line {line} is outside {path}")]
    SourceLineOutOfRange { path: PathBuf, line: u64 },

    #[error("debugger backend thread panicked")]
    BackendThreadPanicked,
    #[error("debugger request was cancelled")]
    RequestCancelled,
    #[error("debugger request queue is closed")]
    RequestQueueClosed,
    #[error("debugger event subscriber fell behind by {0} events")]
    EventStreamLagged(u64),
    #[error("debugger shutdown timed out")]
    ShutdownTimedOut,

    #[error("invalid command: {0}")]
    InvalidCommand(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn debug_info(error: impl StdError + Send + Sync + 'static) -> Self {
        Self::DebugInfo(Box::new(error))
    }

    pub(crate) fn backend(error: impl StdError + Send + Sync + 'static) -> Self {
        Self::Backend(Box::new(error))
    }
}
