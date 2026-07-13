use iced_x86::{Code, Decoder, DecoderOptions, Instruction, Mnemonic, OpKind, Register};

const MAX_PROLOGUE_BYTES: usize = 256;
const MAX_PROLOGUE_INSTRUCTIONS: usize = 64;

/// Explains why an instruction prefix cannot be proven to contain only an
/// x86-64 System V prologue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PrologueAnalysisError {
    Empty,
    TooLong,
    TooManyInstructions,
    InvalidInstruction,
    MissingFramePointerSave,
    MissingFramePointerSetup,
    UnsupportedInstruction,
}

/// Proves that every byte before a candidate source row is a conservative,
/// frame-pointer-based x86-64 System V prologue.
///
/// This deliberately recognizes a small language: the canonical frame-pointer
/// setup, callee-save pushes, constant stack allocation, and unmodified ABI
/// argument copies between registers and frame slots. Any control flow,
/// arithmetic, non-stack memory access, or immediate store is rejected so an
/// optimized function's real work is never skipped merely because it precedes
/// the next line-table row.
pub(super) fn prove_prologue_prefix(
    bytes: &[u8],
    instruction_pointer: u64,
) -> Result<(), PrologueAnalysisError> {
    if bytes.is_empty() {
        return Err(PrologueAnalysisError::Empty);
    }
    if bytes.len() > MAX_PROLOGUE_BYTES {
        return Err(PrologueAnalysisError::TooLong);
    }

    let mut decoder = Decoder::with_ip(64, bytes, instruction_pointer, DecoderOptions::NONE);
    let first = decode(&mut decoder)?;
    let (save, mut count) = if first.mnemonic() == Mnemonic::Endbr64 {
        (decode(&mut decoder)?, 2)
    } else {
        (first, 1)
    };
    if !is_push_register(&save, Register::RBP) {
        return Err(PrologueAnalysisError::MissingFramePointerSave);
    }
    let setup = decode(&mut decoder)?;
    if !is_register_move(&setup, Register::RBP, Register::RSP) {
        return Err(PrologueAnalysisError::MissingFramePointerSetup);
    }

    count += 1;
    let mut tainted = TaintedRegisters::incoming_arguments();

    while decoder.can_decode() {
        count += 1;
        if count > MAX_PROLOGUE_INSTRUCTIONS {
            return Err(PrologueAnalysisError::TooManyInstructions);
        }
        let instruction = decode(&mut decoder)?;

        if is_callee_save_push(&instruction) || is_stack_allocation(&instruction) {
            continue;
        }
        if !is_argument_copy(&instruction, &mut tainted) {
            return Err(PrologueAnalysisError::UnsupportedInstruction);
        }
    }

    Ok(())
}

fn decode(decoder: &mut Decoder<'_>) -> Result<Instruction, PrologueAnalysisError> {
    if !decoder.can_decode() {
        return Err(PrologueAnalysisError::InvalidInstruction);
    }
    let instruction = decoder.decode();
    if instruction.code() == Code::INVALID {
        return Err(PrologueAnalysisError::InvalidInstruction);
    }
    Ok(instruction)
}

fn is_push_register(instruction: &Instruction, register: Register) -> bool {
    instruction.mnemonic() == Mnemonic::Push
        && instruction.op_count() == 1
        && instruction.op0_kind() == OpKind::Register
        && full_register(instruction.op0_register()) == register
}

fn is_register_move(instruction: &Instruction, destination: Register, source: Register) -> bool {
    instruction.mnemonic() == Mnemonic::Mov
        && instruction.op_count() == 2
        && instruction.op0_kind() == OpKind::Register
        && instruction.op1_kind() == OpKind::Register
        && full_register(instruction.op0_register()) == destination
        && full_register(instruction.op1_register()) == source
}

fn is_callee_save_push(instruction: &Instruction) -> bool {
    [
        Register::RBX,
        Register::R12,
        Register::R13,
        Register::R14,
        Register::R15,
    ]
    .iter()
    .any(|register| is_push_register(instruction, *register))
}

fn is_stack_allocation(instruction: &Instruction) -> bool {
    matches!(instruction.mnemonic(), Mnemonic::Sub | Mnemonic::And)
        && instruction.op_count() == 2
        && instruction.op0_kind() == OpKind::Register
        && full_register(instruction.op0_register()) == Register::RSP
        && is_immediate(instruction.op1_kind())
}

fn is_argument_copy(instruction: &Instruction, tainted: &mut TaintedRegisters) -> bool {
    if !is_copy_mnemonic(instruction.mnemonic()) || instruction.op_count() != 2 {
        return false;
    }

    match (instruction.op0_kind(), instruction.op1_kind()) {
        (OpKind::Register, OpKind::Register) => {
            let source = full_register(instruction.op1_register());
            if !tainted.contains(source) {
                return false;
            }
            tainted.insert(full_register(instruction.op0_register()));
            true
        }
        (OpKind::Memory, OpKind::Register) => {
            tainted.contains(full_register(instruction.op1_register()))
                && is_frame_store(instruction)
        }
        (OpKind::Register, OpKind::Memory) => {
            if !is_stack_argument_load(instruction) {
                return false;
            }
            tainted.insert(full_register(instruction.op0_register()));
            true
        }
        _ => false,
    }
}

const fn is_copy_mnemonic(mnemonic: Mnemonic) -> bool {
    matches!(
        mnemonic,
        Mnemonic::Mov
            | Mnemonic::Movapd
            | Mnemonic::Movaps
            | Mnemonic::Movd
            | Mnemonic::Movdqa
            | Mnemonic::Movdqu
            | Mnemonic::Movq
            | Mnemonic::Movsd
            | Mnemonic::Movss
            | Mnemonic::Movsx
            | Mnemonic::Movsxd
            | Mnemonic::Movupd
            | Mnemonic::Movups
            | Mnemonic::Movzx
            | Mnemonic::Vmovapd
            | Mnemonic::Vmovaps
            | Mnemonic::Vmovd
            | Mnemonic::Vmovdqa
            | Mnemonic::Vmovdqu
            | Mnemonic::Vmovq
            | Mnemonic::Vmovsd
            | Mnemonic::Vmovss
            | Mnemonic::Vmovupd
            | Mnemonic::Vmovups
    )
}

fn is_frame_store(instruction: &Instruction) -> bool {
    if instruction.memory_index() != Register::None {
        return false;
    }

    full_register(instruction.memory_base()) == Register::RBP
        && signed_displacement(instruction) < 0
}

fn is_stack_argument_load(instruction: &Instruction) -> bool {
    if instruction.memory_index() != Register::None {
        return false;
    }

    full_register(instruction.memory_base()) == Register::RBP
        && signed_displacement(instruction) >= 16
}

const fn signed_displacement(instruction: &Instruction) -> i64 {
    instruction.memory_displacement64().cast_signed()
}

const fn is_immediate(kind: OpKind) -> bool {
    matches!(
        kind,
        OpKind::Immediate8
            | OpKind::Immediate8_2nd
            | OpKind::Immediate16
            | OpKind::Immediate32
            | OpKind::Immediate64
            | OpKind::Immediate8to16
            | OpKind::Immediate8to32
            | OpKind::Immediate8to64
            | OpKind::Immediate32to64
    )
}

fn full_register(register: Register) -> Register {
    register.info().full_register()
}

struct TaintedRegisters {
    registers: [Register; MAX_PROLOGUE_INSTRUCTIONS + 16],
    len: usize,
}

impl TaintedRegisters {
    fn incoming_arguments() -> Self {
        let mut registers = Self {
            registers: [Register::None; MAX_PROLOGUE_INSTRUCTIONS + 16],
            len: 0,
        };
        for register in [
            Register::RDI,
            Register::RSI,
            Register::RDX,
            Register::RCX,
            Register::R8,
            Register::R9,
            Register::ZMM0,
            Register::ZMM1,
            Register::ZMM2,
            Register::ZMM3,
            Register::ZMM4,
            Register::ZMM5,
            Register::ZMM6,
            Register::ZMM7,
        ] {
            registers.insert(register);
        }
        registers
    }

    fn contains(&self, register: Register) -> bool {
        self.registers[..self.len].contains(&register)
    }

    fn insert(&mut self, register: Register) {
        if !self.contains(register) {
            self.registers[self.len] = register;
            self.len += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_gcc_o0_parameter_homing_prefix() {
        let bytes = [
            0x55, 0x48, 0x89, 0xe5, 0x89, 0xc8, 0x45, 0x89, 0xc2, 0x45, 0x89, 0xc8, 0xf3, 0x0f,
            0x11, 0x45, 0xd4, 0xf2, 0x0f, 0x11, 0x4d, 0xc8, 0x89, 0xf9, 0x88, 0x4d, 0xec, 0x89,
            0xf1, 0x88, 0x4d, 0xe8, 0x88, 0x55, 0xe4, 0x88, 0x45, 0xe0, 0x44, 0x89, 0xd0, 0x66,
            0x89, 0x45, 0xdc, 0x44, 0x89, 0xc0, 0x66, 0x89, 0x45, 0xd8,
        ];

        assert_eq!(prove_prologue_prefix(&bytes, 0x1129), Ok(()));
    }

    #[test]
    fn accepts_stack_argument_copy_and_callee_save_bookkeeping() {
        let bytes = [
            0x55, 0x48, 0x89, 0xe5, 0x53, 0x48, 0x83, 0xec, 0x20, 0x48, 0x8b, 0x45, 0x10, 0x48,
            0x89, 0x45, 0xf8,
        ];

        assert_eq!(prove_prologue_prefix(&bytes, 0x1000), Ok(()));
    }

    #[test]
    fn accepts_control_flow_enforcement_before_the_canonical_prologue() {
        let bytes = [
            0xf3, 0x0f, 0x1e, 0xfa, 0x55, 0x48, 0x89, 0xe5, 0x89, 0x7d, 0xfc,
        ];

        assert_eq!(prove_prologue_prefix(&bytes, 0x1000), Ok(()));
    }

    #[test]
    fn rejects_gcc_o2_real_work_at_entry() {
        let bytes = [
            0x48, 0xc7, 0x05, 0x7d, 0x2e, 0x00, 0x00, 0x63, 0x00, 0x00, 0x00,
        ];

        assert_eq!(
            prove_prologue_prefix(&bytes, 0x11e0),
            Err(PrologueAnalysisError::MissingFramePointerSave)
        );
    }

    #[test]
    fn rejects_real_work_between_frame_setup_and_candidate() {
        let bytes = [
            0x55, 0x48, 0x89, 0xe5, 0xc7, 0x45, 0xfc, 0x63, 0x00, 0x00, 0x00,
        ];

        assert_eq!(
            prove_prologue_prefix(&bytes, 0x1000),
            Err(PrologueAnalysisError::UnsupportedInstruction)
        );
    }

    #[test]
    fn rejects_control_flow_between_frame_setup_and_candidate() {
        let bytes = [0x55, 0x48, 0x89, 0xe5, 0xeb, 0x00];

        assert_eq!(
            prove_prologue_prefix(&bytes, 0x1000),
            Err(PrologueAnalysisError::UnsupportedInstruction)
        );
    }

    #[test]
    fn rejects_stack_probe_side_effects() {
        let bytes = [
            0x55, 0x48, 0x89, 0xe5, 0x48, 0x81, 0xec, 0x00, 0x10, 0x00, 0x00, 0x48, 0x83, 0x0c,
            0x24, 0x00,
        ];

        assert_eq!(
            prove_prologue_prefix(&bytes, 0x1000),
            Err(PrologueAnalysisError::UnsupportedInstruction)
        );
    }

    #[test]
    fn rejects_unproved_rsp_relative_argument_stores() {
        let bytes = [
            0x55, 0x48, 0x89, 0xe5, 0x48, 0x83, 0xec, 0x20, 0x48, 0x89, 0x3c, 0x24,
        ];

        assert_eq!(
            prove_prologue_prefix(&bytes, 0x1000),
            Err(PrologueAnalysisError::UnsupportedInstruction)
        );
    }

    #[test]
    fn rejects_a_candidate_in_the_middle_of_an_instruction() {
        let bytes = [0x55, 0x48, 0x89];

        assert_eq!(
            prove_prologue_prefix(&bytes, 0x1000),
            Err(PrologueAnalysisError::InvalidInstruction)
        );
    }
}
