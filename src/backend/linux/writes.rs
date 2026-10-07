//! Changing memory, variables, and registers at a stop.

use nix::unistd::Pid;

use crate::protocol::StopId;
use crate::{Error, ProcessId, Result, VirtualAddress};

use super::native::LinuxTraceOps;
use super::{Controller, LinuxError, backend_error, validate_process, validate_public_stop};

/// The most bytes one request writes.
pub(super) const MAX_PUBLIC_MEMORY_WRITE: u64 = 64 * 1024;
const WORD: u64 = 8;

impl<P: LinuxTraceOps> Controller<P> {
    /// Writes bytes into the stopped process and returns how many were
    /// written: all of them, or those before memory that cannot be written.
    /// Installed breakpoint traps stay in place; the bytes they hide become
    /// the bytes written there.
    pub(super) fn write_memory(
        &mut self,
        requested_process: ProcessId,
        stop_id: StopId,
        address: VirtualAddress,
        bytes: &[u8],
    ) -> Result<u64> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_process(inferior, requested_process)?;
        validate_public_stop(inferior, Some(stop_id))?;
        let pid = inferior
            .public_stop
            .as_ref()
            .ok_or(Error::NotStopped)?
            .reader();
        self.write_memory_as(pid, address, bytes)
    }

    /// Writes memory through a stopped thread; see [`Self::write_memory`].
    pub(super) fn write_memory_as(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
        bytes: &[u8],
    ) -> Result<u64> {
        let length = bytes.len() as u64;
        // Scans resumed after a write could follow links it changed, and
        // a task may no longer be where its runtime kept it.
        self.views.forget_scans();
        if let Some(stop) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.public_stop.as_ref())
        {
            stop.task_locators.borrow_mut().clear();
        }
        if length > MAX_PUBLIC_MEMORY_WRITE {
            return Err(Error::MemoryWriteTooLarge {
                requested: length,
                maximum: MAX_PUBLIC_MEMORY_WRITE,
            });
        }
        let end = address
            .get()
            .checked_add(length)
            .ok_or(Error::AddressOverflow)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let mut word = address.get() & !(WORD - 1);
        let mut written = 0;
        while word < end {
            let Ok(current) = self.ptrace.read_word(pid, word) else {
                break;
            };
            let mut physical = current.to_ne_bytes();
            let mut hidden = Vec::new();
            let mut count = 0;
            for (offset, byte) in physical.iter_mut().enumerate() {
                let target = word + offset as u64;
                if !(address.get()..end).contains(&target) {
                    continue;
                }
                let user = bytes[usize::try_from(target - address.get()).expect("bounded")];
                count += 1;
                match inferior.breakpoints.get(&VirtualAddress::new(target)) {
                    Some(site) if site.installed => hidden.push((target, user)),
                    _ => *byte = user,
                }
            }
            if self
                .ptrace
                .write_word(pid, word, u64::from_ne_bytes(physical))
                .is_err()
            {
                break;
            }
            for (target, byte) in hidden {
                if let Some(site) = inferior.breakpoints.get_mut(&VirtualAddress::new(target)) {
                    site.original_byte = byte;
                }
            }
            written += count;
            word += WORD;
        }
        if written == 0 && length != 0 {
            return Err(Error::MemoryNotWritable(address));
        }
        Ok(written)
    }

    /// Writes the low bytes of a general register of the innermost frame.
    pub(super) fn write_register(
        &self,
        pid: Pid,
        register: crate::RegisterId,
        bytes: &[u8],
    ) -> Result<()> {
        self.views.forget_scans();
        let mut registers = self.ptrace.registers(pid)?;
        let slot = super::registers::x86_64_general_register_slot(&mut registers, register)
            .ok_or_else(|| backend_error(LinuxError::UnsupportedRegisterWrite))?;
        let mut value = slot.to_le_bytes();
        value[..bytes.len().min(8)].copy_from_slice(&bytes[..bytes.len().min(8)]);
        *slot = u64::from_le_bytes(value);
        self.ptrace.set_registers(pid, registers)
    }
}
