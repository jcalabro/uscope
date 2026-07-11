use std::collections::BTreeMap;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::mpsc::Receiver;

use nix::libc;
use nix::sys::ptrace;
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::Pid;

use crate::protocol::{Command, StopReason};
use crate::{Error, Result};

struct Breakpoint {
    original_byte: u8,
}

struct Inferior {
    pid: Pid,
    load_bias: u64,
    breakpoints: BTreeMap<u64, Breakpoint>,
    stopped_at: Option<u64>,
}

struct Worker {
    executable: PathBuf,
    inferior: Option<Inferior>,
    pending_breakpoints: Vec<(u64, bool)>,
}

pub fn run(executable: PathBuf, commands: &Receiver<Command>) {
    let mut worker = Worker {
        executable,
        inferior: None,
        pending_breakpoints: Vec::new(),
    };
    while let Ok(command) = commands.recv() {
        let shutdown = matches!(command, Command::Shutdown { .. });
        worker.handle(command);
        if shutdown {
            break;
        }
    }
    let _ = worker.kill_inferior();
}

impl Worker {
    fn handle(&mut self, command: Command) {
        match command {
            Command::AddBreakpoint {
                address,
                relocate,
                reply,
            } => {
                let result = self.add_breakpoint(address, relocate);
                let _ = reply.send(result);
            }
            Command::Launch { reply } => {
                let result = self.launch();
                let _ = reply.send(result);
            }
            Command::Continue { reply } => {
                let result = self.resume();
                let _ = reply.send(result);
            }
            Command::ReadWord { address, reply } => {
                let result = self.read_word(address);
                let _ = reply.send(result);
            }
            Command::Relocate {
                link_address,
                reply,
            } => {
                let result = self
                    .inferior
                    .as_ref()
                    .ok_or(Error::NotRunning)
                    .and_then(|inferior| {
                        inferior
                            .load_bias
                            .checked_add(link_address)
                            .ok_or(Error::AddressOverflow)
                    });
                let _ = reply.send(result);
            }
            Command::Shutdown { reply } => {
                let result = self.kill_inferior();
                let _ = reply.send(result);
            }
        }
    }

    fn add_breakpoint(&mut self, address: u64, relocate: bool) -> Result<()> {
        if let Some(inferior) = self.inferior.as_mut() {
            let runtime = if relocate {
                inferior
                    .load_bias
                    .checked_add(address)
                    .ok_or(Error::AddressOverflow)?
            } else {
                address
            };
            return inferior.install_breakpoint(runtime);
        }
        if !self.pending_breakpoints.contains(&(address, relocate)) {
            self.pending_breakpoints.push((address, relocate));
        }
        Ok(())
    }

    fn launch(&mut self) -> Result<StopReason> {
        if self.inferior.is_some() {
            return Err(Error::AlreadyRunning);
        }
        let mut command = ProcessCommand::new(&self.executable);
        trace_child(&mut command);
        let child = command.spawn()?;
        let pid = Pid::from_raw(i32::try_from(child.id()).map_err(|_| Error::AddressOverflow)?);
        match waitpid(pid, None)? {
            WaitStatus::Stopped(_, Signal::SIGTRAP) => {}
            status => return Err(Error::UnexpectedWait(format!("{status:?}"))),
        }
        let load_bias = match load_bias(pid, &self.executable) {
            Ok(load_bias) => load_bias,
            Err(error) => {
                let _ = signal::kill(pid, Signal::SIGKILL);
                let _ = waitpid(pid, None);
                return Err(error);
            }
        };
        let mut inferior = Inferior {
            pid,
            load_bias,
            breakpoints: BTreeMap::new(),
            stopped_at: None,
        };
        for &(address, relocate) in &self.pending_breakpoints {
            let runtime = if relocate {
                load_bias
                    .checked_add(address)
                    .ok_or(Error::AddressOverflow)?
            } else {
                address
            };
            inferior.install_breakpoint(runtime)?;
        }
        self.inferior = Some(inferior);
        self.resume()
    }

    fn resume(&mut self) -> Result<StopReason> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        if let Some(address) = inferior.stopped_at.take() {
            ptrace::step(inferior.pid, None)?;
            match waitpid(inferior.pid, None)? {
                WaitStatus::Stopped(_, Signal::SIGTRAP) => inferior.enable_breakpoint(address)?,
                status => return finish_status(status),
            }
        }
        ptrace::cont(inferior.pid, None)?;
        let status = waitpid(inferior.pid, None)?;
        let result = match status {
            WaitStatus::Stopped(_, Signal::SIGTRAP) => {
                let mut registers = ptrace::getregs(inferior.pid)?;
                let address = registers.rip.checked_sub(1).ok_or(Error::AddressOverflow)?;
                if inferior.breakpoints.contains_key(&address) {
                    inferior.disable_breakpoint(address)?;
                    registers.rip = address;
                    ptrace::setregs(inferior.pid, registers)?;
                    inferior.stopped_at = Some(address);
                    StopReason::Breakpoint { address }
                } else {
                    StopReason::Signal(Signal::SIGTRAP)
                }
            }
            other => finish_status(other)?,
        };
        if matches!(result, StopReason::Exited(_) | StopReason::Signaled(_)) {
            self.inferior = None;
        }
        Ok(result)
    }

    fn read_word(&self, address: u64) -> Result<u64> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let value = ptrace::read(inferior.pid, address as ptrace::AddressType)?;
        Ok(u64::from_ne_bytes(value.to_ne_bytes()))
    }

    fn kill_inferior(&mut self) -> Result<()> {
        let Some(inferior) = self.inferior.take() else {
            return Ok(());
        };
        match signal::kill(inferior.pid, Signal::SIGKILL) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
            Err(error) => return Err(error.into()),
        }
        match waitpid(inferior.pid, None) {
            Ok(_) | Err(nix::errno::Errno::ECHILD) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[allow(
    unsafe_code,
    reason = "pre_exec is the only way to request PTRACE_TRACEME in the child"
)]
fn trace_child(command: &mut ProcessCommand) {
    unsafe {
        command.pre_exec(|| ptrace::traceme().map_err(std::io::Error::other));
    }
}

impl Inferior {
    fn install_breakpoint(&mut self, address: u64) -> Result<()> {
        if self.breakpoints.contains_key(&address) {
            return Ok(());
        }
        let word = read_word(self.pid, address)?;
        let original_byte = word.to_ne_bytes()[0];
        let trap_word = (word & !0xff) | 0xcc;
        ptrace_write(self.pid, address, trap_word)?;
        self.breakpoints
            .insert(address, Breakpoint { original_byte });
        Ok(())
    }

    fn disable_breakpoint(&mut self, address: u64) -> Result<()> {
        let breakpoint = self
            .breakpoints
            .get_mut(&address)
            .expect("known breakpoint");
        let word = read_word(self.pid, address)?;
        ptrace_write(
            self.pid,
            address,
            (word & !0xff) | u64::from(breakpoint.original_byte),
        )?;
        Ok(())
    }

    fn enable_breakpoint(&self, address: u64) -> Result<()> {
        assert!(self.breakpoints.contains_key(&address), "known breakpoint");
        let word = read_word(self.pid, address)?;
        ptrace_write(self.pid, address, (word & !0xff) | 0xcc)?;
        Ok(())
    }
}

fn ptrace_write(pid: Pid, address: u64, value: u64) -> Result<()> {
    let value = libc::c_long::from_ne_bytes(value.to_ne_bytes());
    ptrace::write(pid, address as ptrace::AddressType, value)?;
    Ok(())
}

fn read_word(pid: Pid, address: u64) -> Result<u64> {
    let value = ptrace::read(pid, address as ptrace::AddressType)?;
    Ok(u64::from_ne_bytes(value.to_ne_bytes()))
}

fn finish_status(status: WaitStatus) -> Result<StopReason> {
    match status {
        WaitStatus::Exited(_, code) => Ok(StopReason::Exited(code)),
        WaitStatus::Signaled(_, signal, _) => Ok(StopReason::Signaled(signal)),
        WaitStatus::Stopped(_, signal) => Ok(StopReason::Signal(signal)),
        other => Err(Error::UnexpectedWait(format!("{other:?}"))),
    }
}

fn load_bias(pid: Pid, executable: &Path) -> Result<u64> {
    let maps = fs::read_to_string(format!("/proc/{pid}/maps"))?;
    let executable = executable.to_string_lossy();
    for line in maps.lines() {
        let mut fields = line.split_whitespace();
        let Some(range) = fields.next() else { continue };
        let _permissions = fields.next();
        let Some(offset) = fields.next() else {
            continue;
        };
        let _device = fields.next();
        let _inode = fields.next();
        let Some(path) = fields.next() else { continue };
        if path != executable || offset != "00000000" {
            continue;
        }
        let Some(start) = range.split('-').next() else {
            continue;
        };
        return u64::from_str_radix(start, 16)
            .map_err(|_| Error::LoadBias(executable.as_ref().into()));
    }
    Err(Error::LoadBias(executable.as_ref().into()))
}
