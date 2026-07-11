mod error;
mod protocol;
mod symbols;
mod worker;

pub use error::{Error, Result};
pub use protocol::{BreakpointSpec, StopReason};
pub use symbols::Symbols;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};

use protocol::Command;

pub struct Debugger {
    executable: PathBuf,
    symbols: Symbols,
    commands: SyncSender<Command>,
    worker: Option<JoinHandle<()>>,
}

impl Debugger {
    pub fn new(executable: impl AsRef<Path>) -> Result<Self> {
        let executable = executable.as_ref().canonicalize()?;
        let symbols = Symbols::load(&executable)?;

        let (commands, receiver) = mpsc::sync_channel(32);
        let worker_executable = executable.clone();
        let worker = thread::Builder::new()
            .name("uscope-ptrace".into())
            .spawn(move || worker::run(worker_executable, &receiver))?;

        Ok(Self {
            executable,
            symbols,
            commands,
            worker: Some(worker),
        })
    }

    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn add_breakpoint(&self, spec: BreakpointSpec) -> Result<u64> {
        let (address, relocate) = match spec {
            BreakpointSpec::Address(address) => (address, false),
            BreakpointSpec::Function(name) => (self.symbols.function_address(&name)?, true),
        };

        self.request(|reply| Command::AddBreakpoint {
            address,
            relocate,
            reply,
        })?;

        Ok(address)
    }

    pub fn run(&self) -> Result<StopReason> {
        self.request(|reply| Command::Launch { reply })
    }

    pub fn resume(&self) -> Result<StopReason> {
        self.request(|reply| Command::Continue { reply })
    }

    pub fn read_word(&self, address: u64) -> Result<u64> {
        self.request(|reply| Command::ReadWord { address, reply })
    }

    pub fn runtime_address(&self, name: &str) -> Result<u64> {
        let link_address = self.symbols.symbol_address(name)?;

        self.request(|reply| Command::Relocate {
            link_address,
            reply,
        })
    }

    fn request<T>(&self, make: impl FnOnce(SyncSender<Result<T>>) -> Command) -> Result<T> {
        let (send, receive) = mpsc::sync_channel(1);

        self.commands
            .send(make(send))
            .map_err(|_| Error::WorkerStopped)?;

        receive.recv().map_err(|_| Error::WorkerStopped)?
    }

    pub fn shutdown(&mut self) -> Result<()> {
        if let Some(worker) = self.worker.take() {
            let result = self.request(|reply| Command::Shutdown { reply });
            worker.join().map_err(|_| Error::WorkerPanicked)?;

            result
        } else {
            Ok(())
        }
    }
}

impl Drop for Debugger {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
