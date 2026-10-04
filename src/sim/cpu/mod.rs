//! The simulated CPU: executes one x86-64 instruction of one thread at a
//! time against its registers and address space.
//!
//! iced-x86 decodes; [`ops`] gives each instruction its meaning. Only the
//! instructions the golden corpus executes are implemented, each checked
//! against the real CPU by the lockstep test (`conformance::cpu`). Any other
//! instruction is reported as [`Outcome::Unsupported`], never guessed. The
//! CPU knows nothing of ptrace or signals: the kernel turns each
//! [`Outcome`] into what Linux would do.

mod flags;
mod ops;

use iced_x86::{
    Decoder, DecoderOptions, Formatter as _, GasFormatter, Instruction, Mnemonic, Register,
};

use super::memory::{AddressSpace, MemoryFault};

/// The longest x86-64 instruction.
const MAX_INSTRUCTION_BYTES: usize = 15;

pub const SIGILL: i32 = 4;
pub const SIGFPE: i32 = 8;
pub const SIGSEGV: i32 = 11;
/// `si_code` of an invalid opcode.
const ILL_ILLOPN: i32 = 2;
/// `si_code` of an integer division by zero or overflow.
const FPE_INTDIV: i32 = 1;
/// `si_code` of a fault the kernel raised without an address, such as a
/// general-protection fault.
pub const SI_KERNEL: i32 = 0x80;

pub const CARRY: u64 = 1 << 0;
pub const PARITY: u64 = 1 << 2;
pub const ADJUST: u64 = 1 << 4;
pub const ZERO: u64 = 1 << 6;
pub const SIGN: u64 = 1 << 7;
pub const TRAP: u64 = 1 << 8;
pub const DIRECTION: u64 = 1 << 10;
pub const OVERFLOW: u64 = 1 << 11;
/// The flags arithmetic instructions compute.
pub const STATUS_FLAGS: u64 = CARRY | PARITY | ADJUST | ZERO | SIGN | OVERFLOW;

/// General registers in encoding order, as indices into
/// [`Registers::general`].
pub const RAX: usize = 0;
pub const RCX: usize = 1;
pub const RDX: usize = 2;
pub const RSP: usize = 4;
pub const RBP: usize = 5;
pub const RSI: usize = 6;
pub const RDI: usize = 7;
pub const R11: usize = 11;

/// One thread's user-visible register state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Registers {
    /// RAX, RCX, RDX, RBX, RSP, RBP, RSI, RDI, R8 to R15.
    pub general: [u64; 16],
    pub rip: u64,
    pub rflags: u64,
    pub fs_base: u64,
    pub gs_base: u64,
}

/// A signal the CPU raised for an instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault {
    pub signal: i32,
    pub code: i32,
    pub address: Option<u64>,
}

impl From<MemoryFault> for Fault {
    fn from(fault: MemoryFault) -> Self {
        Self {
            signal: SIGSEGV,
            code: fault.code,
            address: Some(fault.address),
        }
    }
}

/// What executing one instruction did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The instruction ran; `rip` names the next one.
    Completed,
    /// A `syscall`: `rip` is past it, `rcx` and `r11` hold what the CPU
    /// saved, and the kernel must now serve the call.
    Syscall,
    /// An `int3`, with `rip` past it.
    Breakpoint,
    /// The instruction faulted without changing anything; `rip` names it.
    Fault(Fault),
    /// The simulator does not model this instruction, described as text.
    Unsupported(String),
}

/// How a completed instruction moved between functions, which the kernel's
/// shadow call stacks follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Other,
    /// A call, which pushed `return_address` at `slot`.
    Call {
        return_address: u64,
        slot: u64,
    },
    /// A return, to the address now in `rip`.
    Return,
}

/// The effect of an instruction that did not complete normally.
enum Stop {
    Fault(Fault),
    Unsupported,
}

impl From<MemoryFault> for Stop {
    fn from(fault: MemoryFault) -> Self {
        Self::Fault(fault.into())
    }
}

/// Executes the instruction at `registers.rip`.
pub fn step(registers: &mut Registers, memory: &mut AddressSpace) -> Outcome {
    execute(registers, memory).0
}

/// Executes the instruction at `registers.rip`, and says whether it was a
/// call or a return.
pub fn execute(registers: &mut Registers, memory: &mut AddressSpace) -> (Outcome, Flow) {
    let instruction = match decode(registers.rip, memory) {
        Ok(instruction) => instruction,
        Err(fault) => return (Outcome::Fault(fault), Flow::Other),
    };
    let mut next = *registers;
    next.rip = instruction.next_ip();
    match ops::execute(&instruction, &mut next, memory) {
        Ok(outcome) => {
            *registers = next;
            let flow = match instruction.mnemonic() {
                Mnemonic::Call => Flow::Call {
                    return_address: instruction.next_ip(),
                    slot: registers.general[RSP],
                },
                Mnemonic::Ret => Flow::Return,
                _ => Flow::Other,
            };
            (outcome, flow)
        }
        Err(Stop::Fault(fault)) => (Outcome::Fault(fault), Flow::Other),
        Err(Stop::Unsupported) => (Outcome::Unsupported(describe(&instruction)), Flow::Other),
    }
}

/// Decodes the instruction at `address`, or the fault fetching or decoding
/// it raises.
pub fn decode(address: u64, memory: &AddressSpace) -> Result<Instruction, Fault> {
    let mut bytes = [0; MAX_INSTRUCTION_BYTES];
    let fetched = memory.fetch(address, &mut bytes)?;
    let mut decoder = Decoder::with_ip(64, &bytes[..fetched], address, DecoderOptions::NONE);
    let instruction = decoder.decode();
    match decoder.last_error() {
        iced_x86::DecoderError::None => Ok(instruction),
        iced_x86::DecoderError::NoMoreBytes => {
            // The instruction runs into memory that cannot be executed.
            let end = address + fetched as u64;
            Err(memory
                .fetch(end, &mut [0])
                .err()
                .map_or_else(|| invalid_opcode(address), Fault::from))
        }
        _ => Err(invalid_opcode(address)),
    }
}

const fn invalid_opcode(address: u64) -> Fault {
    Fault {
        signal: SIGILL,
        code: ILL_ILLOPN,
        address: Some(address),
    }
}

/// An instruction in AT&T syntax, with its address.
#[must_use]
pub fn describe(instruction: &Instruction) -> String {
    let mut text = format!("{:#x}: ", instruction.ip());
    GasFormatter::new().format(instruction, &mut text);
    text
}

/// The index into [`Registers::general`] of a general register of any
/// size, and whether it names bits 8 to 15 (AH, CH, DH, or BH).
fn general_index(register: Register) -> Option<(usize, bool)> {
    let high = matches!(
        register,
        Register::AH | Register::CH | Register::DH | Register::BH
    );
    let full = register.full_register();
    let index = (full as usize).checked_sub(Register::RAX as usize)?;
    (index < 16 && full.is_gpr64()).then_some((index, high))
}

impl Registers {
    /// Reads a general register of any size, zero-extended.
    fn read(&self, register: Register) -> Option<u64> {
        let (index, high) = general_index(register)?;
        let value = self.general[index];
        Some(if high {
            (value >> 8) & 0xff
        } else {
            value & flags::mask(register.size() * 8)
        })
    }

    /// Writes a general register of any size. A 32-bit write clears the
    /// upper half; narrower writes keep the rest of the register.
    fn write(&mut self, register: Register, value: u64) -> Option<()> {
        let (index, high) = general_index(register)?;
        let slot = &mut self.general[index];
        *slot = match (register.size(), high) {
            (8, _) => value,
            (4, _) => value & 0xffff_ffff,
            (1, true) => (*slot & !0xff00) | ((value & 0xff) << 8),
            (size, _) => {
                let mask = flags::mask(size * 8);
                (*slot & !mask) | (value & mask)
            }
        };
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::memory::{Backing, Protection};

    /// Runs `code` at a fixed address from `registers` until it completes
    /// one instruction, returning the outcome.
    fn run(code: &[u8], registers: &mut Registers) -> Outcome {
        let mut memory = AddressSpace::default();
        memory.map(0x1000, 0x2000, Protection::READ_EXECUTE, Backing::Anonymous);
        memory.map(0x8000, 0x9000, Protection::READ_WRITE, Backing::Anonymous);
        assert!(memory.poke(
            0x1000,
            u64::from_le_bytes(code.try_into().expect("8 bytes"))
        ));
        registers.rip = 0x1000;
        step(registers, &mut memory)
    }

    /// A faulting instruction changes nothing, and the fault names the
    /// address the program touched; a division by zero faults at the
    /// instruction. Decoding never runs off executable memory.
    #[test]
    fn faults_leave_registers_untouched() {
        // push %rax with the stack pointer in unmapped memory.
        let mut registers = Registers::default();
        registers.general[RSP] = 0x4000;
        let before = registers;
        assert_eq!(
            run(&[0x50, 0, 0, 0, 0, 0, 0, 0], &mut registers),
            Outcome::Fault(Fault {
                signal: SIGSEGV,
                code: crate::sim::memory::SEGV_MAPERR,
                address: Some(0x3ff8),
            })
        );
        assert_eq!(
            registers,
            Registers {
                rip: 0x1000,
                ..before
            }
        );

        // div %rcx with rcx zero.
        let mut registers = Registers::default();
        assert_eq!(
            run(&[0x48, 0xf7, 0xf1, 0, 0, 0, 0, 0], &mut registers),
            Outcome::Fault(Fault {
                signal: SIGFPE,
                code: FPE_INTDIV,
                address: Some(0x1000),
            })
        );

        // A write to code is refused.
        let mut registers = Registers::default();
        registers.general[RAX] = 0x1000;
        assert!(matches!(
            run(&[0xc6, 0x00, 0xcc, 0, 0, 0, 0, 0], &mut registers),
            Outcome::Fault(Fault {
                code: crate::sim::memory::SEGV_ACCERR,
                ..
            })
        ));
    }
}
