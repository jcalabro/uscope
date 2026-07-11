use std::error::Error as StdError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("ELF error: {0}")]
    Object(#[from] object::Error),
    #[error("DWARF error: {0}")]
    Dwarf(#[from] gimli::Error),
    #[error("debugger backend error: {0}")]
    Backend(#[source] Box<dyn StdError + Send + Sync>),

    #[error("no function named '{0}' was found")]
    FunctionNotFound(String),
    #[error("multiple functions named '{0}' were found")]
    DuplicateFunction(String),
    #[error("no symbol named '{0}' was found")]
    SymbolNotFound(String),
    #[error("multiple symbols named '{0}' were found")]
    DuplicateSymbol(String),

    #[error("the inferior is already running")]
    AlreadyRunning,
    #[error("the inferior has not been launched")]
    NotRunning,
    #[error("the inferior is not stopped")]
    NotStopped,
    #[error("address arithmetic overflow")]
    AddressOverflow,

    #[error("debugger backend thread panicked")]
    BackendThreadPanicked,
    #[error("debugger request was cancelled")]
    RequestCancelled,
    #[error("debugger request queue is closed")]
    RequestQueueClosed,
    #[error("debugger shutdown timed out")]
    ShutdownTimedOut,

    #[error("invalid command: {0}")]
    InvalidCommand(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn backend(error: impl StdError + Send + Sync + 'static) -> Self {
        Self::Backend(Box::new(error))
    }
}
