//! The interpreter, single-stepped beside the real CPU.
//!
//! Each golden program starts natively under ptrace, stopped at its first
//! instruction. Its registers and memory are copied into the interpreter,
//! then both execute one instruction at a time. After each, the general
//! registers, `rip`, and the flags the instruction defines must agree; the
//! flags it leaves undefined are copied from the CPU. At a system call the
//! kernel's result is copied in and writable memory must agree. The test
//! fails at the first instruction that diverges, and prints it.

use iced_x86::{Instruction, RflagsBits};
use nix::libc;

use crate::backend::sim_edge::NativeTracee;
use crate::sim::corpus::{Corpus, Variant};
use crate::sim::cpu::{
    self, ADJUST, CARRY, DIRECTION, OVERFLOW, Outcome, PARITY, R11, RAX, RDI, Registers, SIGN,
    STATUS_FLAGS, TRAP, ZERO,
};
use crate::sim::kernel::WaitStatus;
use crate::sim::memory::{AddressSpace, Backing, Protection};

/// More instructions than any golden run executes.
const MAX_STEPS: u64 = 1_000_000;
const SYS_EXIT_GROUP: u64 = 231;
/// The resume flag, which the CPU may set as an instruction completes.
const RESUME: u64 = 1 << 16;

/// Every golden program, in every variant and with every argument list its
/// manifest names, executes identically in the interpreter and on the CPU.
#[test]
fn the_interpreter_executes_every_golden_program_as_the_cpu_does() {
    let corpus = Corpus::load().expect("load the golden corpus");
    let mut checked = 0;
    for program in &corpus.programs {
        for variant in &program.variants {
            for run in &program.runs {
                match lockstep(variant, &run.arguments) {
                    Ok(steps) => checked += steps,
                    Err(divergence) => panic!(
                        "{} {:?} diverged from the CPU: {divergence}",
                        variant.name, run.arguments
                    ),
                }
            }
        }
    }
    assert!(checked > 0, "no instruction was checked");
}

/// Runs one program in lockstep to its exit, returning how many
/// instructions were compared.
fn lockstep(variant: &Variant, arguments: &[String]) -> Result<u64, String> {
    let (native, first) = NativeTracee::spawn(&variant.file, arguments);
    if first != WaitStatus::Stopped(native.pid(), libc::SIGTRAP) {
        return Err(format!("the first stop was {first}"));
    }
    let mut registers = native.registers().map_err(|error| error.to_string())?;
    let mut memory = copy_memory(&native);

    for step in 1..=MAX_STEPS {
        let instruction = cpu::decode(registers.rip, &memory)
            .map_err(|fault| format!("decoding at {:#x} faulted: {fault:?}", registers.rip))?;
        let before = registers;
        let outcome = cpu::step(&mut registers, &mut memory);
        native
            .resume(None, true)
            .map_err(|error| error.to_string())?;
        let status = native.wait();
        let at = cpu::describe(&instruction);
        match outcome {
            Outcome::Completed => {}
            Outcome::Syscall => {
                if before.general[RAX] == SYS_EXIT_GROUP {
                    #[expect(clippy::cast_possible_truncation, reason = "exit codes are ints")]
                    let code = (before.general[RDI] as i32) & 0xff;
                    return if status == WaitStatus::Exited(native.pid(), code) {
                        Ok(step)
                    } else {
                        Err(format!(
                            "{at}: exit_group({code}), but the CPU reported {status}"
                        ))
                    };
                }
                // The kernel served the call natively; take its result.
                registers.general[RAX] = native
                    .registers()
                    .map_err(|error| error.to_string())?
                    .general[RAX];
            }
            other => return Err(format!("{at}: the interpreter reported {other:?}")),
        }
        if status != WaitStatus::Stopped(native.pid(), libc::SIGTRAP) {
            return Err(format!("{at}: the CPU reported {status}"));
        }
        let expected = native.registers().map_err(|error| error.to_string())?;
        compare(
            &instruction,
            outcome == Outcome::Syscall,
            &mut registers,
            &expected,
        )
        .map_err(|difference| format!("{at}: {difference}"))?;
        if outcome == Outcome::Syscall {
            compare_memory(&native, &memory).map_err(|difference| format!("{at}: {difference}"))?;
        }
    }
    Err(format!("still running after {MAX_STEPS} instructions"))
}

/// Compares the interpreter's registers with the CPU's after
/// `instruction`, then copies in the flags it leaves undefined.
fn compare(
    instruction: &Instruction,
    after_syscall: bool,
    registers: &mut Registers,
    expected: &Registers,
) -> Result<(), String> {
    for (index, (&actual, &wanted)) in registers.general.iter().zip(&expected.general).enumerate() {
        // r11 saves rflags across `syscall`, including the trap flag
        // single-stepping set.
        let mask = if after_syscall && index == R11 {
            !(TRAP | RESUME)
        } else {
            u64::MAX
        };
        if actual & mask != wanted & mask {
            return Err(format!(
                "general register {index} is {actual:#x}, the CPU has {wanted:#x}"
            ));
        }
    }
    if registers.rip != expected.rip {
        return Err(format!(
            "rip is {:#x}, the CPU has {:#x}",
            registers.rip, expected.rip
        ));
    }
    let undefined = undefined_flags(instruction);
    let defined = (STATUS_FLAGS | DIRECTION) & !undefined;
    if registers.rflags & defined != expected.rflags & defined {
        return Err(format!(
            "rflags is {:#x}, the CPU has {:#x}, comparing {defined:#x}",
            registers.rflags, expected.rflags
        ));
    }
    registers.rflags = (registers.rflags & !undefined) | (expected.rflags & undefined);
    if after_syscall {
        // Keep the trap flag the CPU saved, which later moves of r11 copy.
        registers.general[R11] = expected.general[R11];
    }
    Ok(())
}

/// The flags an instruction leaves undefined, as `rflags` bits.
fn undefined_flags(instruction: &Instruction) -> u64 {
    let iced = instruction.rflags_undefined();
    [
        (RflagsBits::CF, CARRY),
        (RflagsBits::PF, PARITY),
        (RflagsBits::AF, ADJUST),
        (RflagsBits::ZF, ZERO),
        (RflagsBits::SF, SIGN),
        (RflagsBits::OF, OVERFLOW),
    ]
    .into_iter()
    .filter(|&(bit, _)| iced & bit != 0)
    .fold(0, |flags, (_, flag)| flags | flag)
}

/// The tracee's memory, mapped as `/proc/<pid>/maps` describes it.
fn copy_memory(native: &NativeTracee) -> AddressSpace {
    let mut memory = AddressSpace::default();
    for region in regions(native) {
        if region.name == "[vsyscall]" {
            continue;
        }
        memory.map(
            region.start,
            region.end,
            region.protection,
            Backing::Anonymous,
        );
        let length = usize::try_from(region.end - region.start).expect("a mapping fits usize");
        // The kernel refuses reads of some special mappings, such as vvar.
        if let Some(bytes) = native.read_memory(region.start, length) {
            assert!(
                memory.poke_bytes(region.start, &bytes),
                "the region was just mapped"
            );
        }
    }
    memory
}

/// Requires the interpreter's writable memory to equal the tracee's.
fn compare_memory(native: &NativeTracee, memory: &AddressSpace) -> Result<(), String> {
    for region in regions(native).filter(|region| region.protection.write) {
        let length = usize::try_from(region.end - region.start).expect("a mapping fits usize");
        let Some(wanted) = native.read_memory(region.start, length) else {
            continue;
        };
        let actual = memory
            .peek_bytes(region.start, length)
            .ok_or_else(|| format!("{:#x} is not mapped in the interpreter", region.start))?;
        if let Some(offset) = actual.iter().zip(&wanted).position(|(a, b)| a != b) {
            return Err(format!(
                "memory at {:#x} is {:#x}, the CPU has {:#x}",
                region.start + offset as u64,
                actual[offset],
                wanted[offset]
            ));
        }
    }
    Ok(())
}

struct Region {
    start: u64,
    end: u64,
    protection: Protection,
    name: String,
}

fn regions(native: &NativeTracee) -> impl Iterator<Item = Region> {
    native
        .maps()
        .lines()
        .map(|line| {
            let mut fields = line.split_whitespace();
            let range = fields.next().expect("a range");
            let permissions = fields.next().expect("permissions").as_bytes();
            let name = fields.nth(3).unwrap_or("").to_owned();
            let (start, end) = range.split_once('-').expect("a range");
            Region {
                start: u64::from_str_radix(start, 16).expect("a start"),
                end: u64::from_str_radix(end, 16).expect("an end"),
                protection: Protection {
                    read: permissions[0] == b'r',
                    write: permissions[1] == b'w',
                    execute: permissions[2] == b'x',
                },
                name,
            }
        })
        .collect::<Vec<_>>()
        .into_iter()
}
