//! Target memory access that hides installed breakpoint traps.

use std::collections::BTreeMap;

use nix::unistd::Pid;

use crate::protocol::{ProcessId, StopId};
use crate::unwind::MemoryReader;
use crate::{
    Error, MemoryRead, MemoryReadCompletion, MemoryReadUnavailableReason, Result, VirtualAddress,
};

use super::native::{InspectionOps, LinuxTraceOps};
use super::{
    BREAKPOINT_OPCODE, BreakpointSite, Controller, LinuxError, MAX_LOGICAL_MEMORY_READ,
    MAX_PUBLIC_MEMORY_READ, backend_error, validate_process, validate_public_stop,
};

impl<P: InspectionOps> Controller<P> {
    pub(super) fn read_word(
        &self,
        requested_process: ProcessId,
        stop_id: StopId,
        address: VirtualAddress,
    ) -> Result<u64> {
        let read = self.read_memory(
            requested_process,
            stop_id,
            address,
            u64::try_from(std::mem::size_of::<u64>()).expect("native word size fits u64"),
        )?;
        if let MemoryReadCompletion::Incomplete { next_address, .. } = read.completion {
            return Err(backend_error(LinuxError::MemoryInaccessible {
                address: next_address,
            }));
        }
        Ok(u64::from_le_bytes(
            read.bytes
                .as_ref()
                .try_into()
                .expect("one complete native word was requested"),
        ))
    }

    pub(super) fn read_memory(
        &self,
        requested_process: ProcessId,
        stop_id: StopId,
        address: VirtualAddress,
        byte_count: u64,
    ) -> Result<MemoryRead> {
        if byte_count > MAX_PUBLIC_MEMORY_READ {
            return Err(Error::MemoryReadTooLarge {
                requested: byte_count,
                maximum: MAX_PUBLIC_MEMORY_READ,
            });
        }
        address
            .get()
            .checked_add(byte_count)
            .ok_or(Error::AddressOverflow)?;
        let size = usize::try_from(byte_count).expect("bounded memory read size fits usize");
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_process(inferior, requested_process)?;
        validate_public_stop(inferior, Some(stop_id))?;
        let pid = inferior.selected_thread.ok_or(Error::NotStopped)?;
        let read = read_logical_memory(&self.ptrace, pid, &inferior.breakpoints, address, size)?;
        Ok(MemoryRead {
            revision: self.revision,
            stop_id,
            target: self.module_image.target(),
            address,
            requested: byte_count,
            bytes: read.bytes.into(),
            completion: read.completion,
        })
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn write_word(
        &mut self,
        requested_process: ProcessId,
        stop_id: StopId,
        address: VirtualAddress,
        value: u64,
    ) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        validate_process(inferior, requested_process)?;
        validate_public_stop(inferior, Some(stop_id))?;
        let pid = inferior.selected_thread.ok_or(Error::NotStopped)?;
        let user_bytes = value.to_ne_bytes();
        let end = VirtualAddress::new(address.get().saturating_add(user_bytes.len() as u64));

        // Installed traps stay in memory; the user's bytes become the bytes
        // they hide, recorded only once the write succeeds.
        let mut physical_bytes = user_bytes;
        let mut hidden = Vec::new();
        for (&site_address, site) in inferior.breakpoints.range(address..end) {
            if site.installed {
                let offset = usize::try_from(site_address.get() - address.get())
                    .expect("word offset fits usize");
                hidden.push((site_address, user_bytes[offset]));
                physical_bytes[offset] = BREAKPOINT_OPCODE;
            }
        }
        self.ptrace
            .write_word(pid, address.get(), u64::from_ne_bytes(physical_bytes))?;
        for (site_address, byte) in hidden {
            if let Some(site) = inferior.breakpoints.get_mut(&site_address) {
                site.original_byte = byte;
            }
        }
        Ok(())
    }
}

pub(super) struct PtraceMemory<'a> {
    pub(super) ptrace: &'a dyn InspectionOps,
    pub(super) pid: Pid,
}

pub(super) struct LogicalMemoryRead {
    pub(super) bytes: Vec<u8>,
    pub(super) completion: MemoryReadCompletion,
}

pub(super) enum MemoryAccessError {
    Inaccessible,
    /// Only the first `readable` bytes of the word are accessible, such as
    /// where a core dump's file-backed memory ends inside a word.
    Partial {
        word: u64,
        readable: usize,
    },
    Fatal(Error),
}

pub(super) fn read_logical_memory(
    ptrace: &impl InspectionOps,
    pid: Pid,
    breakpoints: &BTreeMap<VirtualAddress, BreakpointSite>,
    address: VirtualAddress,
    size: usize,
) -> Result<LogicalMemoryRead> {
    read_logical_memory_with(address, size, breakpoints, |current| {
        ptrace.read_memory_word(pid, current)
    })
}

pub(super) fn read_logical_memory_with(
    address: VirtualAddress,
    size: usize,
    breakpoints: &BTreeMap<VirtualAddress, BreakpointSite>,
    mut read_word: impl FnMut(u64) -> std::result::Result<u64, MemoryAccessError>,
) -> Result<LogicalMemoryRead> {
    if size > MAX_LOGICAL_MEMORY_READ {
        return Err(backend_error(LinuxError::MemoryReadTooLarge {
            size,
            maximum: MAX_LOGICAL_MEMORY_READ,
        }));
    }
    if size == 0 {
        return Ok(LogicalMemoryRead {
            bytes: Vec::new(),
            completion: MemoryReadCompletion::Complete,
        });
    }
    let end = address
        .get()
        .checked_add(u64::try_from(size).expect("memory read size fits u64"))
        .ok_or(Error::AddressOverflow)?;
    let mut bytes = Vec::with_capacity(size);
    let word_size = u64::try_from(std::mem::size_of::<u64>()).expect("word size fits u64");
    let mut current = address.get() & !(word_size - 1);
    while current < end {
        let (mut word, readable) = match read_word(current) {
            Ok(word) => (word.to_le_bytes(), word_size),
            Err(MemoryAccessError::Partial { word, readable }) => (
                word.to_le_bytes(),
                u64::try_from(readable).unwrap_or(u64::MAX).min(word_size),
            ),
            Err(MemoryAccessError::Inaccessible) => (0_u64.to_le_bytes(), 0),
            Err(MemoryAccessError::Fatal(error)) => return Err(error),
        };
        let last_word_address = current.saturating_add(word_size - 1);
        for (&site_address, site) in
            breakpoints.range(VirtualAddress::new(current)..=VirtualAddress::new(last_word_address))
        {
            let offset = site_address.get() - current;
            if site.installed {
                word[usize::try_from(offset).expect("word offset fits usize")] = site.original_byte;
            }
        }
        let readable_end = current.saturating_add(readable);
        let selected_start = address.get().max(current);
        let selected_end = end.min(readable_end);
        if selected_start < selected_end {
            let start = usize::try_from(selected_start - current).expect("word offset fits usize");
            let stop = usize::try_from(selected_end - current).expect("word offset fits usize");
            bytes.extend_from_slice(&word[start..stop]);
        }
        if readable < word_size && readable_end < end {
            return Ok(LogicalMemoryRead {
                completion: MemoryReadCompletion::Incomplete {
                    next_address: VirtualAddress::new(address.get().max(readable_end)),
                    reason: MemoryReadUnavailableReason::Inaccessible,
                },
                bytes,
            });
        }
        if current.saturating_add(word_size) >= end {
            break;
        }
        current = current
            .checked_add(word_size)
            .ok_or(Error::AddressOverflow)?;
    }
    Ok(LogicalMemoryRead {
        bytes,
        completion: MemoryReadCompletion::Complete,
    })
}

impl MemoryReader for PtraceMemory<'_> {
    fn read_u64(&mut self, address: VirtualAddress) -> Option<u64> {
        self.ptrace.read_word(self.pid, address.get()).ok()
    }
}
