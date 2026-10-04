//! The harness the kernel's conformance tests share: one script of ptrace
//! operations, run on a real traced process and on the simulated kernel,
//! recording what it observes in terms that do not depend on where things
//! happen to be, so the two records can be compared.
//!
//! Threads run concurrently on Linux, so a script observes only what does
//! not depend on how they interleave: it waits for one named thread's next
//! status, and holds threads in stops where a race would matter.

use std::collections::BTreeMap;
use std::sync::Arc;

use nix::errno::Errno;
use nix::libc;
use object::{Object as _, ObjectSection as _, ObjectSymbol as _};

use crate::backend::native_tracee::NativeTracee;
use crate::backend::sim_edge::signal_view;
use crate::sim::corpus::{Corpus, Variant};
use crate::sim::cpu::{RAX, Registers};
use crate::sim::kernel::{Kernel, Options, Tid, WaitStatus};

/// The simulated debugger's process identifier.
const TRACER: i32 = 100;
/// How many instructions a simulated wait may run before it gives up.
const MAX_WAIT_STEPS: u64 = 10_000_000;

/// One traced process, real or simulated, as a script drives it. Requests
/// name the thread they address.
pub(super) trait Tracee {
    fn leader(&self) -> Tid;
    fn tracer(&self) -> i32;
    /// Waits for `thread`'s next status.
    fn wait(&mut self, thread: Tid) -> WaitStatus;
    /// Whether `thread` has a status to report, without reaping it.
    fn has_report(&mut self, thread: Tid) -> bool;
    /// The registers, and `orig_rax`.
    fn registers(&self, thread: Tid) -> Result<(Registers, u64), Errno>;
    fn set_registers(&mut self, thread: Tid, registers: &Registers) -> Result<(), Errno>;
    /// The signal, code, sender, and fault address, as the controller sees
    /// them.
    fn signal(&self, thread: Tid) -> Result<(i32, i32, Option<i32>, Option<u64>), Errno>;
    fn event_message(&self, thread: Tid) -> Result<u64, Errno>;
    fn set_options(&mut self, thread: Tid) -> Result<(), Errno>;
    fn resume(&mut self, thread: Tid, signal: Option<i32>, single_step: bool) -> Result<(), Errno>;
    fn kill(&mut self) -> Result<(), Errno>;
    fn request_stop(&mut self, thread: Tid) -> Result<(), Errno>;
    fn peek(&self, thread: Tid, address: u64) -> Result<u64, Errno>;
    fn poke(&mut self, thread: Tid, address: u64, value: u64) -> Result<(), Errno>;
    fn name(&self, thread: Tid) -> String;
    /// The thread group `/proc` names, or `None` once the thread is gone.
    fn thread_group(&self, thread: Tid) -> Option<Tid>;
    /// `/proc/<thread>/maps`.
    fn maps(&self, thread: Tid) -> Option<String>;
}

impl Tracee for NativeTracee {
    fn leader(&self) -> Tid {
        self.pid()
    }
    fn tracer(&self) -> i32 {
        i32::try_from(std::process::id()).expect("a process identifier fits i32")
    }
    fn wait(&mut self, thread: Tid) -> WaitStatus {
        Self::wait(self, thread)
    }
    fn has_report(&mut self, thread: Tid) -> bool {
        Self::has_report(self, thread)
    }
    fn registers(&self, thread: Tid) -> Result<(Registers, u64), Errno> {
        Ok((
            Self::registers(self, thread)?,
            Self::system_call(self, thread)?,
        ))
    }
    fn set_registers(&mut self, thread: Tid, registers: &Registers) -> Result<(), Errno> {
        Self::set_registers(self, thread, registers)
    }
    fn signal(&self, thread: Tid) -> Result<(i32, i32, Option<i32>, Option<u64>), Errno> {
        self.signal_view(thread)
    }
    fn event_message(&self, thread: Tid) -> Result<u64, Errno> {
        Self::event_message(self, thread)
    }
    fn set_options(&mut self, thread: Tid) -> Result<(), Errno> {
        Self::set_options(self, thread, true)
    }
    fn resume(&mut self, thread: Tid, signal: Option<i32>, single_step: bool) -> Result<(), Errno> {
        Self::resume(self, thread, signal, single_step)
    }
    fn kill(&mut self) -> Result<(), Errno> {
        Self::kill(self, libc::SIGKILL)
    }
    fn request_stop(&mut self, thread: Tid) -> Result<(), Errno> {
        Self::request_stop(self, thread)
    }
    fn peek(&self, thread: Tid, address: u64) -> Result<u64, Errno> {
        Self::peek(self, thread, address)
    }
    fn poke(&mut self, thread: Tid, address: u64, value: u64) -> Result<(), Errno> {
        Self::poke(self, thread, address, value)
    }
    fn name(&self, thread: Tid) -> String {
        std::fs::read_to_string(format!("/proc/{}/task/{thread}/comm", self.pid()))
            .expect("read the thread's name")
            .trim_end()
            .to_owned()
    }
    fn thread_group(&self, thread: Tid) -> Option<Tid> {
        Self::thread_group(self, thread)
    }
    fn maps(&self, thread: Tid) -> Option<String> {
        Self::maps(self, thread)
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
        let first = tracee.wait(pid);
        (tracee, first)
    }

    fn check_gap(&self) {
        if let Some(gap) = &self.kernel.gap {
            panic!("model gap: {}", gap.0);
        }
    }
}

impl Tracee for SimTracee {
    fn leader(&self) -> Tid {
        self.pid
    }
    fn tracer(&self) -> i32 {
        TRACER
    }
    /// Runs every thread that can run, a few instructions each in turn,
    /// until `thread` reports.
    fn wait(&mut self, thread: Tid) -> WaitStatus {
        let mut steps = 0;
        loop {
            if let Some(status) = self.kernel.collect(thread) {
                return status;
            }
            let runnable = self.kernel.runnable().collect::<Vec<_>>();
            assert!(
                !runnable.is_empty(),
                "{thread} can never report: no simulated thread can run"
            );
            for tid in runnable {
                steps += self.kernel.run(tid, 16).executed.max(1);
                self.check_gap();
            }
            assert!(steps < MAX_WAIT_STEPS, "{thread} never reported");
        }
    }
    fn has_report(&mut self, thread: Tid) -> bool {
        self.kernel.reportable().any(|tid| tid == thread)
    }
    fn registers(&self, thread: Tid) -> Result<(Registers, u64), Errno> {
        self.kernel.get_registers_and_call(thread)
    }
    fn set_registers(&mut self, thread: Tid, registers: &Registers) -> Result<(), Errno> {
        self.kernel.set_registers(thread, *registers)
    }
    fn signal(&self, thread: Tid) -> Result<(i32, i32, Option<i32>, Option<u64>), Errno> {
        self.kernel.signal_info(thread).map(signal_view)
    }
    fn event_message(&self, thread: Tid) -> Result<u64, Errno> {
        self.kernel.event_message(thread)
    }
    fn set_options(&mut self, thread: Tid) -> Result<(), Errno> {
        self.kernel.set_options(
            thread,
            Options {
                trace_clone: true,
                trace_exit: true,
                exit_kill: true,
            },
        )
    }
    fn resume(&mut self, thread: Tid, signal: Option<i32>, single_step: bool) -> Result<(), Errno> {
        self.kernel.resume(thread, signal, single_step)
    }
    fn kill(&mut self) -> Result<(), Errno> {
        self.kernel.kill(self.pid, libc::SIGKILL)
    }
    fn request_stop(&mut self, thread: Tid) -> Result<(), Errno> {
        self.kernel.tgkill(self.pid, thread, libc::SIGSTOP)
    }
    fn peek(&self, thread: Tid, address: u64) -> Result<u64, Errno> {
        self.kernel.peek(thread, address)
    }
    fn poke(&mut self, thread: Tid, address: u64, value: u64) -> Result<(), Errno> {
        self.kernel.poke(thread, address, value)
    }
    fn name(&self, thread: Tid) -> String {
        self.kernel
            .process_of(thread)
            .expect("the thread exists")
            .name
            .to_string()
    }
    fn thread_group(&self, thread: Tid) -> Option<Tid> {
        self.kernel.thread_group(thread)
    }
    fn maps(&self, thread: Tid) -> Option<String> {
        self.kernel.maps(thread)
    }
}

/// Addresses a script needs in one variant, and symbols that describe
/// others.
pub(super) struct Landmarks {
    pub(super) entry: u64,
    /// The first `syscall` instruction.
    pub(super) syscall: u64,
    /// An address nothing maps.
    pub(super) unmapped: u64,
    symbols: BTreeMap<&'static str, u64>,
}

/// Symbols whose addresses describe others, where a program defines them.
const SYMBOLS: [&str; 7] = [
    "fib",
    "share",
    "tick",
    "rt_clone",
    "rt_thread_start",
    "stacks",
    "rt_exit",
];

impl Landmarks {
    pub(super) fn of(variant: &Variant) -> Self {
        let file = object::File::parse(&*variant.data).expect("parse the golden binary");
        let symbols = SYMBOLS
            .into_iter()
            .filter_map(|name| {
                let symbol = file.symbols().find(|symbol| symbol.name() == Ok(name))?;
                Some((name, symbol.address()))
            })
            .collect();
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
            syscall,
            unmapped: 0x1000,
            symbols,
        }
    }

    pub(super) fn symbol(&self, name: &str) -> u64 {
        *self
            .symbols
            .get(name)
            .unwrap_or_else(|| panic!("the program defines no {name}"))
    }

    /// Describes `address` relative to the landmarks.
    pub(super) fn describe(&self, address: u64) -> String {
        [("entry", self.entry), ("syscall", self.syscall)]
            .into_iter()
            .chain(self.symbols.iter().map(|(&name, &address)| (name, address)))
            .filter(|&(_, landmark)| address >= landmark && address - landmark < 64)
            .map(|(name, landmark)| format!("{name}+{}", address - landmark))
            .next()
            .unwrap_or_else(|| "elsewhere".to_owned())
    }
}

/// Records what a script observes.
pub(super) struct Record<'a> {
    pub(super) tracee: &'a mut dyn Tracee,
    pub(super) landmarks: &'a Landmarks,
    /// Threads by creation order, the leader first.
    threads: Vec<Tid>,
    lines: Vec<String>,
}

impl Record<'_> {
    pub(super) fn note(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
    }

    pub(super) fn leader(&self) -> Tid {
        self.tracee.leader()
    }

    /// A thread's name in the record, which does not depend on its id.
    pub(super) fn name_of(&self, tid: Tid) -> String {
        match self.threads.iter().position(|&known| known == tid) {
            Some(0) => "leader".to_owned(),
            Some(index) => format!("thread {index}"),
            None if tid == self.tracee.tracer() => "tracer".to_owned(),
            None => format!("unknown {}", if tid == 0 { "0" } else { "thread" }),
        }
    }

    pub(super) fn wait(&mut self, thread: Tid) -> WaitStatus {
        let status = self.tracee.wait(thread);
        let text = status
            .to_string()
            .replacen(&thread.to_string(), &self.name_of(thread), 1);
        self.note(format!("wait {}: {text}", self.name_of(thread)));
        status
    }

    /// The stopped thread's signal information and location.
    pub(super) fn stop(&mut self, thread: Tid) {
        let signal = self
            .tracee
            .signal(thread)
            .map(|(signal, code, sender, address)| {
                let sender = sender.map(|sender| self.name_of(sender));
                let address = address.map(|address| self.landmarks.describe(address));
                format!("signal {signal} code {code:#x} sender {sender:?} address {address:?}")
            });
        let name = self.name_of(thread);
        self.note(format!("siginfo of {name}: {signal:?}"));
        let rip = self
            .tracee
            .registers(thread)
            .map(|(registers, _)| self.landmarks.describe(registers.rip));
        self.note(format!("rip of {name}: {rip:?}"));
    }

    /// Where a stopped thread is in a system call: `rax` and `orig_rax`.
    pub(super) fn system_call(&mut self, thread: Tid) {
        let call = self.tracee.registers(thread).map(|(registers, orig_rax)| {
            (registers.general[RAX].cast_signed(), orig_rax.cast_signed())
        });
        let name = self.name_of(thread);
        self.note(format!("rax and orig_rax of {name}: {call:?}"));
    }

    pub(super) fn event_message(&mut self, thread: Tid) {
        let message = self.tracee.event_message(thread);
        let name = self.name_of(thread);
        self.note(format!("event message of {name}: {message:x?}"));
    }

    /// Handles a creator's clone event: records it and names the new
    /// thread, which is returned.
    pub(super) fn cloned(&mut self, creator: Tid) -> Tid {
        self.wait(creator);
        self.stop(creator);
        self.system_call(creator);
        let child = self
            .tracee
            .event_message(creator)
            .map(|message| Tid::try_from(message).expect("a thread id"))
            .expect("the clone event names the new thread");
        self.threads.push(child);
        let group = self.tracee.thread_group(child);
        let same = group == Some(self.leader());
        let name = self.name_of(child);
        self.note(format!("{name} is in the creator's group: {same}"));
        child
    }

    pub(super) fn result(&mut self, operation: &str, result: Result<(), Errno>) {
        self.note(format!("{operation}: {result:?}"));
    }

    pub(super) fn set_options(&mut self, thread: Tid) {
        let result = self.tracee.set_options(thread);
        let name = self.name_of(thread);
        self.result(&format!("set options of {name}"), result);
    }

    pub(super) fn resume(&mut self, thread: Tid, signal: Option<i32>) {
        let result = self.tracee.resume(thread, signal, false);
        let name = self.name_of(thread);
        self.result(&format!("continue {name} with {signal:?}"), result);
    }

    pub(super) fn step(&mut self, thread: Tid) {
        let result = self.tracee.resume(thread, None, true);
        let name = self.name_of(thread);
        self.result(&format!("step {name}"), result);
    }

    pub(super) fn has_report(&mut self, thread: Tid) {
        let reported = self.tracee.has_report(thread);
        let name = self.name_of(thread);
        self.note(format!("{name} has a report: {reported}"));
    }

    /// Plants `int3` at `address`, returning the original word.
    pub(super) fn plant(&mut self, address: u64) -> u64 {
        let leader = self.leader();
        let original = self.tracee.peek(leader, address).expect("read code");
        let result = self.tracee.poke(leader, address, (original & !0xff) | 0xcc);
        self.result("poke int3", result);
        original
    }

    /// Restores a planted word through `thread`.
    pub(super) fn restore(&mut self, thread: Tid, address: u64, original: u64) {
        let result = self.tracee.poke(thread, address, original);
        self.result("restore", result);
    }

    /// Rewinds a thread's `rip` over the trap it executed.
    pub(super) fn rewind(&mut self, thread: Tid) {
        let (mut registers, _) = self.tracee.registers(thread).expect("registers at a trap");
        registers.rip -= 1;
        let result = self.tracee.set_registers(thread, &registers);
        let name = self.name_of(thread);
        self.result(&format!("rewind {name}"), result);
    }
}

/// Runs `script` natively and simulated on every variant of `program`
/// with `arguments`, and requires the same observations.
pub(super) fn dual_run(program: &str, arguments: &[&str], script: impl Fn(&mut Record<'_>)) {
    let corpus = Corpus::load().expect("load the golden corpus");
    let program = corpus
        .programs
        .iter()
        .find(|candidate| candidate.name == program)
        .unwrap_or_else(|| panic!("no golden program {program}"));
    let arguments = arguments
        .iter()
        .map(|&argument| argument.to_owned())
        .collect::<Vec<_>>();
    for variant in &program.variants {
        let landmarks = Landmarks::of(variant);
        let observe = |tracee: &mut dyn Tracee, first: WaitStatus| {
            let leader = tracee.leader();
            let mut record = Record {
                tracee,
                landmarks: &landmarks,
                threads: vec![leader],
                lines: Vec::new(),
            };
            record.note(format!("first: {first:?}").replace(&leader.to_string(), "leader"));
            script(&mut record);
            record.lines
        };
        let (mut native, first) = NativeTracee::spawn(&variant.file, &arguments);
        let native_lines = observe(&mut native, first);
        drop(native);
        let (mut simulated, first) = SimTracee::spawn(variant, &arguments);
        let simulated_lines = observe(&mut simulated, first);
        assert_eq!(
            simulated_lines, native_lines,
            "{} behaves differently simulated (left) and on Linux (right)",
            variant.name
        );
    }
}
