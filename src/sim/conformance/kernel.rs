//! The simulated kernel's rules, each checked against Linux.
//!
//! Every test runs one script of ptrace operations twice on each golden
//! variant: once on a real traced process, once on the simulated kernel.
//! Both runs record what they observe, in terms that do not depend on
//! where things happen to be (statuses, errnos, signal information, and
//! addresses relative to known symbols), and the records must be equal. A
//! rule the simulation models without such a test is a guess.

use std::sync::Arc;

use nix::errno::Errno;
use nix::libc;
use object::{Object as _, ObjectSection as _, ObjectSymbol as _};

use crate::backend::sim_edge::{NativeTracee, signal_view};
use crate::sim::corpus::{Corpus, Variant};
use crate::sim::cpu::{RAX, Registers};
use crate::sim::kernel::{Kernel, Options, Tid, WaitStatus};

/// The simulated debugger's process identifier.
const TRACER: i32 = 100;
/// How many instructions a simulated wait may run before it gives up.
const MAX_WAIT_STEPS: u64 = 10_000_000;

/// One traced process, real or simulated, as a script drives it.
trait Tracee {
    fn pid(&self) -> Tid;
    fn tracer(&self) -> i32;
    /// Waits for the next status.
    fn wait(&mut self) -> WaitStatus;
    fn registers(&self) -> Result<Registers, Errno>;
    fn set_registers(&mut self, registers: &Registers) -> Result<(), Errno>;
    /// The signal, code, sender, and fault address, as the controller sees
    /// them.
    fn signal(&self) -> Result<(i32, i32, Option<i32>, Option<u64>), Errno>;
    fn event_message(&self) -> Result<u64, Errno>;
    fn set_options(&mut self) -> Result<(), Errno>;
    fn resume(&mut self, signal: Option<i32>, single_step: bool) -> Result<(), Errno>;
    fn kill(&mut self) -> Result<(), Errno>;
    fn request_stop(&mut self) -> Result<(), Errno>;
    fn peek(&self, address: u64) -> Result<u64, Errno>;
    fn poke(&mut self, address: u64, value: u64) -> Result<(), Errno>;
    fn name(&self) -> String;
}

impl Tracee for NativeTracee {
    fn pid(&self) -> Tid {
        Self::pid(self)
    }
    fn tracer(&self) -> i32 {
        i32::try_from(std::process::id()).expect("a process identifier fits i32")
    }
    fn wait(&mut self) -> WaitStatus {
        Self::wait(self)
    }
    fn registers(&self) -> Result<Registers, Errno> {
        Self::registers(self)
    }
    fn set_registers(&mut self, registers: &Registers) -> Result<(), Errno> {
        Self::set_registers(self, registers)
    }
    fn signal(&self) -> Result<(i32, i32, Option<i32>, Option<u64>), Errno> {
        self.signal_view()
    }
    fn event_message(&self) -> Result<u64, Errno> {
        Self::event_message(self)
    }
    fn set_options(&mut self) -> Result<(), Errno> {
        Self::set_options(self, true)
    }
    fn resume(&mut self, signal: Option<i32>, single_step: bool) -> Result<(), Errno> {
        Self::resume(self, signal, single_step)
    }
    fn kill(&mut self) -> Result<(), Errno> {
        Self::kill(self, libc::SIGKILL)
    }
    fn request_stop(&mut self) -> Result<(), Errno> {
        Self::request_stop(self)
    }
    fn peek(&self, address: u64) -> Result<u64, Errno> {
        Self::peek(self, address)
    }
    fn poke(&mut self, address: u64, value: u64) -> Result<(), Errno> {
        Self::poke(self, address, value)
    }
    fn name(&self) -> String {
        std::fs::read_to_string(format!("/proc/{}/comm", self.pid()))
            .expect("read the thread's name")
            .trim_end()
            .to_owned()
    }
}

/// The simulated kernel running one golden program.
struct SimTracee {
    kernel: Kernel,
    pid: Tid,
}

impl SimTracee {
    fn spawn(variant: &Variant, arguments: &[String]) -> (Self, WaitStatus) {
        let mut kernel = Kernel::new(TRACER);
        let pid = kernel.spawn(
            Arc::clone(&variant.image),
            &variant.path,
            arguments,
            [0; 16],
        );
        let mut tracee = Self { kernel, pid };
        let first = tracee.wait();
        (tracee, first)
    }
}

impl Tracee for SimTracee {
    fn pid(&self) -> Tid {
        self.pid
    }
    fn tracer(&self) -> i32 {
        TRACER
    }
    fn wait(&mut self) -> WaitStatus {
        let mut steps = 0;
        loop {
            if let Some(status) = self.kernel.collect(self.pid) {
                return status;
            }
            assert!(
                self.kernel
                    .threads
                    .get(&self.pid)
                    .is_some_and(crate::sim::kernel::Thread::can_run),
                "the simulated tracee can never report a status"
            );
            steps += self.kernel.run(self.pid, 1000).max(1);
            if let Some(gap) = &self.kernel.gap {
                panic!("model gap: {}", gap.0);
            }
            assert!(
                steps < MAX_WAIT_STEPS,
                "the simulated tracee never reported"
            );
        }
    }
    fn registers(&self) -> Result<Registers, Errno> {
        self.kernel.get_registers(self.pid)
    }
    fn set_registers(&mut self, registers: &Registers) -> Result<(), Errno> {
        self.kernel.set_registers(self.pid, *registers)
    }
    fn signal(&self) -> Result<(i32, i32, Option<i32>, Option<u64>), Errno> {
        self.kernel.signal_info(self.pid).map(signal_view)
    }
    fn event_message(&self) -> Result<u64, Errno> {
        self.kernel.event_message(self.pid)
    }
    fn set_options(&mut self) -> Result<(), Errno> {
        self.kernel.set_options(
            self.pid,
            Options {
                trace_exit: true,
                exit_kill: true,
            },
        )
    }
    fn resume(&mut self, signal: Option<i32>, single_step: bool) -> Result<(), Errno> {
        self.kernel.resume(self.pid, signal, single_step)
    }
    fn kill(&mut self) -> Result<(), Errno> {
        self.kernel.kill(self.pid, libc::SIGKILL)
    }
    fn request_stop(&mut self) -> Result<(), Errno> {
        self.kernel.tgkill(self.pid, self.pid, libc::SIGSTOP)
    }
    fn peek(&self, address: u64) -> Result<u64, Errno> {
        self.kernel.peek(self.pid, address)
    }
    fn poke(&mut self, address: u64, value: u64) -> Result<(), Errno> {
        self.kernel.poke(self.pid, address, value)
    }
    fn name(&self) -> String {
        self.kernel
            .process_of(self.pid)
            .expect("the tracee exists")
            .name
            .to_string()
    }
}

/// Addresses a script needs in one variant.
struct Landmarks {
    entry: u64,
    /// A function the program calls.
    fib: u64,
    /// The first `syscall` instruction.
    syscall: u64,
    /// An address nothing maps.
    unmapped: u64,
}

impl Landmarks {
    fn of(variant: &Variant) -> Self {
        let file = object::File::parse(&*variant.data).expect("parse the golden binary");
        let fib = file
            .symbols()
            .find(|symbol| symbol.name() == Ok("fib"))
            .expect("a fib symbol")
            .address();
        let text = file.section_by_name(".text").expect("a text section");
        let mut decoder = iced_x86::Decoder::with_ip(
            64,
            text.data().expect("text bytes"),
            text.address(),
            iced_x86::DecoderOptions::NONE,
        );
        let syscall = decoder
            .iter()
            .find(|instruction| instruction.mnemonic() == iced_x86::Mnemonic::Syscall)
            .expect("a syscall instruction")
            .ip();
        Self {
            entry: file.entry(),
            fib,
            syscall,
            unmapped: 0x1000,
        }
    }

    /// Describes `address` relative to the landmarks.
    fn describe(&self, address: u64) -> String {
        [
            ("entry", self.entry),
            ("fib", self.fib),
            ("syscall", self.syscall),
        ]
        .into_iter()
        .filter(|&(_, landmark)| address >= landmark && address - landmark < 64)
        .map(|(name, landmark)| format!("{name}+{}", address - landmark))
        .next()
        .unwrap_or_else(|| "elsewhere".to_owned())
    }
}

/// Records what a script observes.
struct Record<'a> {
    tracee: &'a mut dyn Tracee,
    landmarks: &'a Landmarks,
    lines: Vec<String>,
}

impl Record<'_> {
    fn note(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
    }

    fn wait(&mut self) {
        let pid = self.tracee.pid();
        let status = self.tracee.wait();
        let text = status.to_string().replacen(&pid.to_string(), "tracee", 1);
        self.note(format!("wait: {text}"));
    }

    /// The stopped thread's signal information and location.
    fn stop(&mut self) {
        let signal = self.tracee.signal().map(|(signal, code, sender, address)| {
            let sender = sender.map(|sender| {
                if sender == self.tracee.pid() {
                    "tracee".to_owned()
                } else if sender == self.tracee.tracer() {
                    "tracer".to_owned()
                } else {
                    sender.to_string()
                }
            });
            let address = address.map(|address| self.landmarks.describe(address));
            format!("signal {signal} code {code:#x} sender {sender:?} address {address:?}")
        });
        self.note(format!("siginfo: {signal:?}"));
        let rip = self
            .tracee
            .registers()
            .map(|registers| self.landmarks.describe(registers.rip));
        self.note(format!("rip: {rip:?}"));
    }

    fn result(&mut self, operation: &str, result: Result<(), Errno>) {
        self.note(format!("{operation}: {result:?}"));
    }

    /// Plants `int3` at `address`, returning the original word.
    fn plant(&mut self, address: u64) -> u64 {
        let original = self.tracee.peek(address).expect("read code");
        let result = self.tracee.poke(address, (original & !0xff) | 0xcc);
        self.result("poke int3", result);
        original
    }

    /// Restores a planted word and rewinds `rip` over the trap.
    fn lift(&mut self, address: u64, original: u64) {
        let result = self.tracee.poke(address, original);
        self.result("restore", result);
        let mut registers = self.tracee.registers().expect("registers at the trap");
        registers.rip -= 1;
        let result = self.tracee.set_registers(&registers);
        self.result("rewind", result);
    }
}

/// Runs `script` natively and simulated on every golden variant and
/// requires the same observations.
fn dual_run(arguments: &[&str], script: impl Fn(&mut Record<'_>)) {
    let corpus = Corpus::load().expect("load the golden corpus");
    let arguments = arguments
        .iter()
        .map(|&argument| argument.to_owned())
        .collect::<Vec<_>>();
    for variant in corpus.programs.iter().flat_map(|program| &program.variants) {
        let landmarks = Landmarks::of(variant);
        let observe = |tracee: &mut dyn Tracee, first: WaitStatus| {
            let mut record = Record {
                tracee,
                landmarks: &landmarks,
                lines: Vec::new(),
            };
            record.note(
                format!("first: {first:?}").replace(&record.tracee.pid().to_string(), "tracee"),
            );
            script(&mut record);
            record.lines
        };
        let (mut native, first) = NativeTracee::spawn(&variant.file, &arguments);
        let native_lines = observe(&mut native, first);
        let (mut simulated, first) = SimTracee::spawn(variant, &arguments);
        let simulated_lines = observe(&mut simulated, first);
        assert_eq!(
            simulated_lines, native_lines,
            "{} behaves differently simulated (left) and on Linux (right)",
            variant.name
        );
    }
}

/// K-EXEC-1: a launched program first stops at its entry point for a
/// SIGTRAP it sent itself, and is named after its executable.
#[test]
fn k_exec_1_a_launched_program_stops_at_its_entry() {
    dual_run(&[], |record| {
        record.stop();
        let name = record.tracee.name();
        record.note(format!("name: {name}"));
    });
}

/// K-TRAP-1: `int3` reports SIGTRAP with `SI_KERNEL` and `rip` past the
/// trap; a single step reports `TRAP_TRACE` at the next instruction; a
/// single step across `syscall` reports `TRAP_BRKPT`.
#[test]
fn k_trap_1_traps_report_their_kind_and_place() {
    dual_run(&[], |record| {
        let result = record.tracee.set_options();
        record.result("set options", result);
        let fib = record.landmarks.fib;
        let original = record.plant(fib);
        let result = record.tracee.resume(None, false);
        record.result("continue", result);
        record.wait();
        record.stop();
        record.lift(fib, original);
        let result = record.tracee.resume(None, true);
        record.result("step", result);
        record.wait();
        record.stop();

        let syscall = record.landmarks.syscall;
        let original = record.plant(syscall);
        let result = record.tracee.resume(None, false);
        record.result("continue", result);
        record.wait();
        record.lift(syscall, original);
        let result = record.tracee.resume(None, true);
        record.result("step across syscall", result);
        record.wait();
        record.stop();
        let result = record
            .tracee
            .registers()
            .map(|registers| registers.general[RAX]);
        record.note(format!("syscall result: {result:?}"));
    });
}

/// K-EXIT-1, single-threaded: `exit_group` stops at the exit event with
/// the status in its message and `si_code` `0x605`; continuing reports the
/// exit. K-WAIT-1: requests on a thread that is not stopped fail with
/// ESRCH.
#[test]
fn k_exit_1_an_exiting_thread_stops_at_its_exit_event() {
    dual_run(&["1"], |record| {
        let result = record.tracee.set_options();
        record.result("set options", result);
        let result = record.tracee.resume(None, false);
        record.result("continue", result);
        record.wait();
        record.stop();
        let message = record.tracee.event_message();
        record.note(format!("event message: {message:?}"));
        let result = record.tracee.resume(None, false);
        record.result("continue", result);
        record.wait();
        let result = record.tracee.registers().map(drop);
        record.result("registers of a reaped thread", result);
    });
}

/// K-EXIT-3: SIGKILL takes a thread out of a signal-delivery-stop to its
/// exit event, with message 9, and then reports it killed. K-EXIT-4: a
/// thread already at its exit event stays there through another SIGKILL.
#[test]
fn k_exit_3_sigkill_ends_a_stopped_thread_through_its_exit_event() {
    dual_run(&[], |record| {
        let result = record.tracee.set_options();
        record.result("set options", result);
        let fib = record.landmarks.fib;
        record.plant(fib);
        let result = record.tracee.resume(None, false);
        record.result("continue", result);
        record.wait();
        let result = record.tracee.kill();
        record.result("kill", result);
        record.wait();
        record.stop();
        let message = record.tracee.event_message();
        record.note(format!("event message: {message:?}"));
        let result = record.tracee.kill();
        record.result("kill again", result);
        let result = record.tracee.registers().map(drop);
        record.result("registers at the exit event", result);
        let result = record.tracee.resume(None, false);
        record.result("continue", result);
        record.wait();
    });
}

/// K-EXIT-4: a thread at its exit event from `exit_group` stays there
/// through SIGKILL and then reports the exit it was making.
#[test]
fn k_exit_4_sigkill_leaves_a_thread_at_its_exit_event() {
    dual_run(&["0"], |record| {
        let result = record.tracee.set_options();
        record.result("set options", result);
        let result = record.tracee.resume(None, false);
        record.result("continue", result);
        record.wait();
        let result = record.tracee.kill();
        record.result("kill", result);
        record.stop();
        let result = record.tracee.resume(None, false);
        record.result("continue", result);
        record.wait();
    });
}

/// K-SIG-1: SIGSTOP from the tracer's `tgkill` to a stopped thread waits
/// until the thread resumes, then stops it with `SI_TKILL` from the
/// tracer before it runs. Continuing without the signal suppresses it.
#[test]
fn k_sig_1_a_tracer_stop_request_stops_the_thread() {
    dual_run(&[], |record| {
        let result = record.tracee.set_options();
        record.result("set options", result);
        let result = record.tracee.request_stop();
        record.result("tgkill SIGSTOP", result);
        let result = record.tracee.resume(None, false);
        record.result("continue", result);
        record.wait();
        record.stop();
        let result = record.tracee.resume(None, false);
        record.result("continue without the signal", result);
        record.wait();
        record.stop();
    });
}

/// K-MEM-1: ptrace writes ignore page protections and fail with EIO where
/// nothing is mapped, as reads do.
#[test]
fn k_mem_1_ptrace_ignores_protections_but_not_holes() {
    dual_run(&[], |record| {
        let entry = record.landmarks.entry;
        let word = record.tracee.peek(entry).expect("read the entry point");
        let result = record.tracee.poke(entry, word ^ 0xff);
        record.result("poke code", result);
        let changed = record.tracee.peek(entry).map(|changed| changed ^ word);
        record.note(format!("changed: {changed:?}"));
        let unmapped = record.landmarks.unmapped;
        let result = record.tracee.peek(unmapped).map(drop);
        record.result("peek unmapped", result);
        let result = record.tracee.poke(unmapped, 0);
        record.result("poke unmapped", result);
    });
}
