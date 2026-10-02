//! The x86-64 register file in the forms unwinding and inspection consume.

use std::sync::Arc;

use nix::libc;
use nix::unistd::Pid;

use crate::backend::linux::core_dump;
use crate::debug_info::{VariableRegister, VariableRuntimeError};
use crate::unwind::RegisterFile;
use crate::{
    RegisterDescriptor, RegisterId, RegisterRole, RegisterSnapshot, RegisterValue,
    UnsupportedVariableFeature, VariableUnavailableReason,
};

use super::debug_thread_id;

pub(super) fn x86_64_registers(registers: &libc::user_regs_struct) -> RegisterFile {
    RegisterFile::new([
        (0, registers.rax),
        (1, registers.rdx),
        (2, registers.rcx),
        (3, registers.rbx),
        (4, registers.rsi),
        (5, registers.rdi),
        (6, registers.rbp),
        (7, registers.rsp),
        (8, registers.r8),
        (9, registers.r9),
        (10, registers.r10),
        (11, registers.r11),
        (12, registers.r12),
        (13, registers.r13),
        (14, registers.r14),
        (15, registers.r15),
        (16, registers.rip),
        (49, registers.eflags),
    ])
}

pub(super) fn x86_64_general_variable_register(
    registers: &libc::user_regs_struct,
    dwarf: u16,
) -> Option<VariableRegister> {
    let descriptor = x86_64_general_register_descriptor(dwarf)?;
    let value = match dwarf {
        0 => registers.rax,
        1 => registers.rdx,
        2 => registers.rcx,
        3 => registers.rbx,
        4 => registers.rsi,
        5 => registers.rdi,
        6 => registers.rbp,
        7 => registers.rsp,
        8 => registers.r8,
        9 => registers.r9,
        10 => registers.r10,
        11 => registers.r11,
        12 => registers.r12,
        13 => registers.r13,
        14 => registers.r14,
        15 => registers.r15,
        16 => registers.rip,
        49 => registers.eflags,
        _ => return None,
    };
    Some(VariableRegister {
        descriptor,
        bytes: Arc::from(value.to_le_bytes()),
    })
}

/// Reads a register of a caller's activation from the registers the
/// unwinder reconstructed for it.
pub(super) fn x86_64_caller_variable_register(
    registers: &RegisterFile,
    dwarf: u16,
) -> Result<VariableRegister, VariableRuntimeError> {
    if let Some(descriptor) = x86_64_general_register_descriptor(dwarf) {
        return match registers.get(dwarf) {
            Some(value) => Ok(VariableRegister {
                descriptor,
                bytes: Arc::from(value.to_le_bytes()),
            }),
            None => Err(VariableUnavailableReason::RegisterNotSaved(descriptor.name).into()),
        };
    }
    if (17..=32).contains(&dwarf) {
        // Callees may overwrite every SSE register without saving it.
        return Err(VariableUnavailableReason::RegisterNotSaved(
            format!("xmm{}", dwarf - 17).into(),
        )
        .into());
    }
    Err(VariableUnavailableReason::Unsupported(UnsupportedVariableFeature::RegisterClass).into())
}

/// The field of a general register, by its descriptor's id.
pub(super) const fn x86_64_general_register_slot(
    registers: &mut libc::user_regs_struct,
    register: RegisterId,
) -> Option<&mut u64> {
    Some(match register.get() {
        0 => &mut registers.rax,
        1 => &mut registers.rbx,
        2 => &mut registers.rcx,
        3 => &mut registers.rdx,
        4 => &mut registers.rsi,
        5 => &mut registers.rdi,
        6 => &mut registers.rbp,
        7 => &mut registers.rsp,
        8 => &mut registers.r8,
        9 => &mut registers.r9,
        10 => &mut registers.r10,
        11 => &mut registers.r11,
        12 => &mut registers.r12,
        13 => &mut registers.r13,
        14 => &mut registers.r14,
        15 => &mut registers.r15,
        16 => &mut registers.rip,
        17 => &mut registers.eflags,
        _ => return None,
    })
}

pub(super) fn x86_64_general_register_descriptor(dwarf: u16) -> Option<RegisterDescriptor> {
    let (id, name, role) = match dwarf {
        0 => (0, "rax", None),
        1 => (3, "rdx", None),
        2 => (2, "rcx", None),
        3 => (1, "rbx", None),
        4 => (4, "rsi", None),
        5 => (5, "rdi", None),
        6 => (6, "rbp", Some(RegisterRole::FramePointer)),
        7 => (7, "rsp", Some(RegisterRole::StackPointer)),
        8 => (8, "r8", None),
        9 => (9, "r9", None),
        10 => (10, "r10", None),
        11 => (11, "r11", None),
        12 => (12, "r12", None),
        13 => (13, "r13", None),
        14 => (14, "r14", None),
        15 => (15, "r15", None),
        16 => (16, "rip", Some(RegisterRole::ProgramCounter)),
        49 => (17, "rflags", None),
        _ => return None,
    };
    Some(RegisterDescriptor {
        id: RegisterId::new(id),
        name: name.into(),
        bits: 64,
        role,
    })
}

/// The 512-byte x86-64 FXSAVE image saved by ptrace and by `NT_FPREGSET`.
pub(super) type Fxsave = Arc<[u8; core_dump::FXSAVE_SIZE]>;

pub(super) const FXSAVE_XMM_OFFSET: usize = 160;

pub(super) fn native_fxsave(registers: &libc::user_fpregs_struct) -> Fxsave {
    let mut bytes = Vec::with_capacity(core_dump::FXSAVE_SIZE);
    for value in [registers.cwd, registers.swd, registers.ftw, registers.fop] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(&registers.rip.to_le_bytes());
    bytes.extend_from_slice(&registers.rdp.to_le_bytes());
    bytes.extend_from_slice(&registers.mxcsr.to_le_bytes());
    bytes.extend_from_slice(&registers.mxcr_mask.to_le_bytes());
    for word in registers.st_space.iter().chain(&registers.xmm_space) {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    bytes.resize(core_dump::FXSAVE_SIZE, 0);
    Arc::new(bytes.try_into().expect("FXSAVE image has its fixed size"))
}

pub(super) fn x86_64_xmm_variable_register(registers: &Fxsave, dwarf: u16) -> VariableRegister {
    let index = usize::from(dwarf - 17);
    let start = FXSAVE_XMM_OFFSET + index * 16;
    let bytes = registers[start..start + 16].to_vec();
    VariableRegister {
        descriptor: RegisterDescriptor {
            id: RegisterId::new(27 + u32::try_from(index).expect("XMM index fits u32")),
            name: format!("xmm{index}").into(),
            bits: 128,
            role: None,
        },
        bytes: bytes.into(),
    }
}

/// Describes a frame's general registers: the thread's own for the
/// innermost activation, or the registers the unwinder reconstructed for a
/// caller's. A caller's register that a callee may have overwritten without
/// saving it has no value; the segment and thread-pointer registers are the
/// same in every frame, and the system-call register belongs to the thread.
pub(super) fn x86_64_register_snapshot(
    revision: u64,
    pid: Pid,
    target: crate::TargetDescription,
    native: &libc::user_regs_struct,
    caller: Option<&RegisterFile>,
) -> RegisterSnapshot {
    let general = [
        (0, native.rax),
        (3, native.rbx),
        (2, native.rcx),
        (1, native.rdx),
        (4, native.rsi),
        (5, native.rdi),
        (6, native.rbp),
        (7, native.rsp),
        (8, native.r8),
        (9, native.r9),
        (10, native.r10),
        (11, native.r11),
        (12, native.r12),
        (13, native.r13),
        (14, native.r14),
        (15, native.r15),
        (16, native.rip),
        (49, native.eflags),
    ];
    let special = [
        ("cs", 16, true, native.cs),
        ("ss", 16, true, native.ss),
        ("ds", 16, true, native.ds),
        ("es", 16, true, native.es),
        ("fs", 16, true, native.fs),
        ("gs", 16, true, native.gs),
        ("fs_base", 64, true, native.fs_base),
        ("gs_base", 64, true, native.gs_base),
        ("orig_rax", 64, false, native.orig_rax),
    ];
    let registers =
        general
            .into_iter()
            .map(|(dwarf, value)| RegisterValue {
                register: x86_64_general_register_descriptor(dwarf)
                    .expect("snapshot uses supported DWARF registers"),
                bytes: caller
                    .map_or(Some(value), |file| file.get(dwarf))
                    .map(|value| Arc::from(value.to_le_bytes())),
            })
            .chain(special.into_iter().enumerate().map(
                |(offset, (name, bits, every_frame, value))| {
                    let bytes = value.to_le_bytes();
                    let byte_count = usize::from(bits / 8);
                    RegisterValue {
                        register: RegisterDescriptor {
                            id: RegisterId::new(
                                18 + u32::try_from(offset).expect("x86-64 register ID fits u32"),
                            ),
                            name: name.into(),
                            bits,
                            role: None,
                        },
                        bytes: (caller.is_none() || every_frame)
                            .then(|| Arc::from(&bytes[..byte_count])),
                    }
                },
            ))
            .collect::<Vec<_>>()
            .into();
    RegisterSnapshot {
        revision,
        thread: debug_thread_id(pid),
        target,
        registers,
    }
}
