//! The interpreter, single-stepped beside the real CPU.
//!
//! Each golden program starts natively under ptrace, stopped at its first
//! instruction. Its registers and memory are copied into the interpreter,
//! then both execute one instruction at a time. After each, the general
//! registers, `rip`, and the flags the instruction defines must agree; the
//! flags it leaves undefined are copied from the CPU. At a system call the
//! kernel's result is copied in and writable memory must agree. The test
//! fails at the first instruction that diverges, and prints it.
//!
//! Threads take turns: the current one runs until it makes a system call,
//! then the next in creation order runs. Every other thread waits in a
//! ptrace-stop meanwhile, so the program interleaves on the CPU exactly as
//! in the interpreter. A new thread starts from the registers Linux gave
//! it, which the kernel's conformance tests check.

use iced_x86::{Instruction, RflagsBits};
use nix::libc;

use crate::backend::native_tracee::NativeTracee;
use crate::sim::corpus::{Corpus, Variant};
use crate::sim::cpu::{
    self, ADJUST, CARRY, DIRECTION, OVERFLOW, Outcome, PARITY, R11, RAX, RDI, Registers, SIGN,
    STATUS_FLAGS, TRAP, ZERO,
};
use crate::sim::kernel::{Tid, WaitStatus};
use crate::sim::memory::{AddressSpace, Backing, Protection};

/// More instructions than any golden run executes.
const MAX_STEPS: u64 = 1_000_000;
const SYS_CLONE: u64 = 56;
const SYS_EXIT: u64 = 60;
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
    let leader = native.pid();
    if first != WaitStatus::Stopped(leader, libc::SIGTRAP) {
        return Err(format!("the first stop was {first}"));
    }
    let failed = |error: nix::errno::Errno| error.to_string();
    // Trace clones and exits, so that every thread stops where the
    // interpreter's does.
    native.set_options(leader, true).map_err(failed)?;
    let mut threads: Vec<(Tid, Registers)> =
        vec![(leader, native.registers(leader).map_err(failed)?)];
    let mut memory = copy_memory(&native);
    let mut current = 0;

    for step in 1..=MAX_STEPS {
        let (tid, mut registers) = threads[current];
        let instruction = cpu::decode(registers.rip, &memory)
            .map_err(|fault| format!("decoding at {:#x} faulted: {fault:?}", registers.rip))?;
        let before = registers;
        let outcome = cpu::step(&mut registers, &mut memory);
        native.resume(tid, None, true).map_err(failed)?;
        let mut status = native.wait(tid);
        let at = format!("thread {tid}: {}", cpu::describe(&instruction));
        match outcome {
            Outcome::Completed => {}
            Outcome::Syscall => {
                #[expect(clippy::cast_possible_truncation, reason = "exit codes are ints")]
                let code = (before.general[RDI] as i32) & 0xff;
                match before.general[RAX] {
                    SYS_EXIT_GROUP => {
                        return expect_exit_event(&native, tid, status, code)
                            .map(|()| step)
                            .map_err(|difference| format!("{at}: {difference}"));
                    }
                    SYS_EXIT => {
                        expect_exit_event(&native, tid, status, code)
                            .map_err(|difference| format!("{at}: {difference}"))?;
                        native.resume(tid, None, false).map_err(failed)?;
                        threads.remove(current);
                        if tid != leader {
                            let exited = native.wait(tid);
                            if exited != WaitStatus::Exited(tid, code) {
                                return Err(format!("{at}: then it reported {exited}"));
                            }
                        }
                        if threads.is_empty() {
                            // The leader reports last, with how the last
                            // thread ended.
                            let ended = native.wait(leader);
                            return if ended == WaitStatus::Exited(leader, code) {
                                Ok(step)
                            } else {
                                Err(format!("{at}: the process ended {ended}"))
                            };
                        }
                        current %= threads.len();
                        continue;
                    }
                    SYS_CLONE => {
                        if status != WaitStatus::Event(tid, libc::PTRACE_EVENT_CLONE) {
                            return Err(format!("{at}: the CPU reported {status}"));
                        }
                        let child =
                            native
                                .event_message(tid)
                                .map_err(failed)
                                .and_then(|message| {
                                    Tid::try_from(message).map_err(|error| error.to_string())
                                })?;
                        // The step completes at the call's exit.
                        native.resume(tid, None, true).map_err(failed)?;
                        status = native.wait(tid);
                        let started = native.wait(child);
                        if started != WaitStatus::Stopped(child, libc::SIGSTOP) {
                            return Err(format!("{at}: the new thread reported {started}"));
                        }
                        threads.push((child, native.registers(child).map_err(failed)?));
                    }
                    _ => {}
                }
                // The kernel served the call natively; take its result.
                registers.general[RAX] = native.registers(tid).map_err(failed)?.general[RAX];
            }
            other => return Err(format!("{at}: the interpreter reported {other:?}")),
        }
        if status != WaitStatus::Stopped(tid, libc::SIGTRAP) {
            return Err(format!("{at}: the CPU reported {status}"));
        }
        let expected = native.registers(tid).map_err(failed)?;
        compare(
            &instruction,
            outcome == Outcome::Syscall,
            &mut registers,
            &expected,
        )
        .map_err(|difference| format!("{at}: {difference}"))?;
        threads[current].1 = registers;
        if outcome == Outcome::Syscall {
            compare_memory(&native, &memory).map_err(|difference| format!("{at}: {difference}"))?;
            current = (current + 1) % threads.len();
        }
    }
    Err(format!("still running after {MAX_STEPS} instructions"))
}

/// Requires `status` to be `tid`'s exit event for an exit with `code`.
fn expect_exit_event(
    native: &NativeTracee,
    tid: Tid,
    status: WaitStatus,
    code: i32,
) -> Result<(), String> {
    if status != WaitStatus::Event(tid, libc::PTRACE_EVENT_EXIT) {
        return Err(format!("exiting with {code}, the CPU reported {status}"));
    }
    let message = native
        .event_message(tid)
        .map_err(|error| error.to_string())?;
    if message != u64::from(code.cast_unsigned()) << 8 {
        return Err(format!(
            "exiting with {code}, the exit event says {message:#x}"
        ));
    }
    Ok(())
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
        .maps(native.pid())
        .unwrap_or_default()
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
