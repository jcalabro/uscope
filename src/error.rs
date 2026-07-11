use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("ELF error: {0}")]
    Object(#[from] object::Error),
    #[error("DWARF error: {0}")]
    Dwarf(#[from] gimli::Error),
    #[error("ptrace error: {0}")]
    Ptrace(#[from] nix::Error),

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
    #[error("inferior exited with status {0}")]
    InferiorExited(i32),
    #[error("inferior was terminated by signal {0}")]
    InferiorSignaled(nix::sys::signal::Signal),

    #[error("unexpected wait status: {0}")]
    UnexpectedWait(String),
    #[error("could not determine load bias for {0}")]
    LoadBias(PathBuf),
    #[error("address arithmetic overflow")]
    AddressOverflow,

    #[error("debugger worker stopped unexpectedly")]
    WorkerStopped,
    #[error("debugger worker panicked")]
    WorkerPanicked,

    #[error("invalid command: {0}")]
    InvalidCommand(String),
}

pub type Result<T> = std::result::Result<T, Error>;
