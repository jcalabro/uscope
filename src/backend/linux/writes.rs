//! Changing memory, variables, and registers at a stop.

use nix::unistd::Pid;

use crate::condition::{Condition, Operand};
use crate::protocol::StopId;
use crate::{
    Error, InspectedValue, ProcessId, Result, StackFrameId, TypeInfo, TypeKind, ValueExpression,
    VariableState, VariableValueSource, VirtualAddress,
};

use super::native::LinuxTraceOps;
use super::{Controller, LinuxError, backend_error, validate_process, validate_public_stop};

/// The most bytes one request writes.
pub(super) const MAX_PUBLIC_MEMORY_WRITE: u64 = 64 * 1024;
const WORD: u64 = 8;
/// How many typedefs and qualifiers a type may wrap its enumeration in.
const MAX_TYPE_CHAIN: usize = 16;

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
        let pid = inferior.selected_thread.ok_or(Error::NotStopped)?;
        self.write_memory_as(pid, address, bytes)
    }

    /// Writes memory through a stopped thread; see [`Self::write_memory`].
    fn write_memory_as(&mut self, pid: Pid, address: VirtualAddress, bytes: &[u8]) -> Result<u64> {
        let length = bytes.len() as u64;
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

    /// Assigns the value of `text`, an expression evaluated in the same
    /// frame, to the scalar, enumeration, or pointer `expression` names, and
    /// returns its new value.
    pub(super) fn assign(
        &mut self,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
        expression: &ValueExpression,
        text: &str,
    ) -> Result<InspectedValue> {
        let refused = |reason: &str| Error::AssignmentRefused {
            expression: expression.to_string(),
            reason: reason.to_owned(),
        };
        let current = self.inspect(
            stop_id,
            pid,
            frame,
            expression,
            crate::InspectionLimits::default(),
        )?;
        let VariableState::Available {
            source, raw, value, ..
        } = &current.state
        else {
            return Err(refused("its value is not available"));
        };
        let size = raw.as_ref().map_or(0, |raw| raw.len());
        if size == 0 {
            return Err(refused(
                "only numbers, booleans, enumerations, and pointers can be assigned",
            ));
        }
        let operand =
            self.assigned_operand(stop_id, pid, frame, current.type_info.as_ref(), text)?;
        let bytes =
            crate::assign::encode(operand, value, size, self.module_image.target().byte_order)
                .map_err(|reason| refused(&reason))?;
        match source {
            VariableValueSource::Memory(address) => {
                if self.write_memory_as(pid, *address, &bytes)? != bytes.len() as u64 {
                    return Err(Error::MemoryNotWritable(*address));
                }
            }
            VariableValueSource::Register(register) => {
                // A register belongs to the innermost frame; a caller's copy
                // lives in memory its callees saved, and a part of a register
                // cannot be told from the whole.
                if frame != StackFrameId::INNERMOST {
                    return Err(refused("it is held in a register of a caller's frame"));
                }
                if expression.steps.len() != 1 {
                    return Err(refused("it is part of a value held in a register"));
                }
                self.write_register(pid, register.id, &bytes).map_err(|_| {
                    refused(&format!("register {} cannot be changed", register.name))
                })?;
            }
            _ => {
                return Err(refused(
                    "the debug information computes it; it has no storage",
                ));
            }
        }
        self.inspect(
            stop_id,
            pid,
            frame,
            expression,
            crate::InspectionLimits::default(),
        )
    }

    /// Evaluates an assigned value: an enumerator of the target's
    /// enumeration, or an expression in the frame.
    fn assigned_operand(
        &self,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
        type_info: Option<&TypeInfo>,
        text: &str,
    ) -> Result<Operand> {
        if let Some(operand) = type_info
            .and_then(|type_info| self.enumerators(type_info))
            .and_then(|enumerators| crate::assign::enumerator(&enumerators, text.trim()))
        {
            return Ok(operand);
        }
        let condition = Condition::parse(text)?;
        let mut resolve = |path: &ValueExpression| {
            let value = self
                .inspect(
                    stop_id,
                    pid,
                    frame,
                    path,
                    crate::InspectionLimits::default(),
                )
                .map_err(|error| error.to_string())?;
            Operand::of(path, value.type_info.as_ref(), &value.state)
        };
        condition
            .value(&mut resolve)
            .map_err(|reason| Error::AssignmentRefused {
                expression: text.to_owned(),
                reason,
            })
    }

    /// The enumerators of an enumeration type, through typedefs and
    /// qualifiers of the main image.
    fn enumerators(&self, type_info: &TypeInfo) -> Option<std::sync::Arc<[crate::Enumerator]>> {
        let mut current = type_info.clone();
        for _ in 0..MAX_TYPE_CHAIN {
            let next = match &current.kind {
                TypeKind::Enumeration { enumerators, .. } => return Some(enumerators.clone()),
                TypeKind::Modified { target, .. }
                | TypeKind::Named {
                    target: Some(target),
                    ..
                } => *target,
                _ => return None,
            };
            current = self
                .modules
                .values()
                .find(|module| module.image.id() == next.image)?
                .image
                .type_info(next)?
                .clone();
        }
        None
    }

    /// Writes the low bytes of a general register of the innermost frame.
    fn write_register(&self, pid: Pid, register: crate::RegisterId, bytes: &[u8]) -> Result<()> {
        let mut registers = self.ptrace.registers(pid)?;
        let slot = super::registers::x86_64_general_register_slot(&mut registers, register)
            .ok_or_else(|| backend_error(LinuxError::UnsupportedRegisterWrite))?;
        let mut value = slot.to_le_bytes();
        value[..bytes.len().min(8)].copy_from_slice(&bytes[..bytes.len().min(8)]);
        *slot = u64::from_le_bytes(value);
        self.ptrace.set_registers(pid, registers)
    }
}
