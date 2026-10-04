//! The simulated Linux kernel: processes, threads, signals, ptrace, and
//! wait statuses, following the rules in `plans/simulator.md` (K-*).
//!
//! The kernel contains no randomness. Wherever Linux leaves an order open,
//! such as which thread runs next or which status a wait returns, it
//! exposes the options and the world decides. Requests it does not model
//! fail as model gaps, never with a plausible default.
//!
//! - [`ptrace`]: the tracer's requests and their errno outcomes.
//! - [`signals`]: generation, delivery, and exits.
//! - [`syscalls`]: the system calls the golden runtime makes.

pub mod ptrace;
pub mod signals;
mod syscalls;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use super::cpu::{self, Outcome, Registers};
use super::loader::Image;
use super::memory::AddressSpace;
use signals::{SIGTRAP, SignalSet};

/// A thread or process identifier.
pub type Tid = i32;

/// The identifier the first simulated process gets.
const FIRST_TID: Tid = 1000;

/// What `waitpid` reports for a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitStatus {
    Exited(Tid, i32),
    /// Killed by a signal, and whether it dumped core.
    Signaled(Tid, i32, bool),
    /// A signal-delivery-stop.
    Stopped(Tid, i32),
    /// A `PTRACE_EVENT_*` stop.
    Event(Tid, i32),
}

impl WaitStatus {
    #[must_use]
    pub const fn tid(self) -> Tid {
        match self {
            Self::Exited(tid, _)
            | Self::Signaled(tid, _, _)
            | Self::Stopped(tid, _)
            | Self::Event(tid, _) => tid,
        }
    }
}

impl fmt::Display for WaitStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Exited(tid, code) => write!(formatter, "{tid} exited {code}"),
            Self::Signaled(tid, signal, core) => write!(
                formatter,
                "{tid} killed by {}{}",
                signals::name(signal),
                if core { " (core)" } else { "" }
            ),
            Self::Stopped(tid, signal) => {
                write!(formatter, "{tid} stopped {}", signals::name(signal))
            }
            Self::Event(tid, event) => {
                write!(formatter, "{tid} event {}", ptrace::event_name(event))
            }
        }
    }
}

/// The fields of the `siginfo_t` `PTRACE_GETSIGINFO` reads. Which of
/// `pid` and `address` mean anything depends on the signal and code, as in
/// the real structure, whose union overlays them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SigInfo {
    pub signal: i32,
    pub code: i32,
    /// `si_pid`: the sending process, for codes that name one.
    pub pid: i32,
    /// `si_addr`: the faulting address, for synchronous faults.
    pub address: u64,
}

/// How a thread or process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    Code(i32),
    /// The signal that killed it, and whether it dumped core.
    Signal(i32, bool),
}

impl ExitStatus {
    /// The message `PTRACE_GETEVENTMSG` reports at the exit event, which is
    /// the status `waitpid` will report.
    const fn event_message(self) -> u64 {
        match self {
            Self::Code(code) => ((code & 0xff).cast_unsigned() as u64) << 8,
            Self::Signal(signal, core) => {
                signal.cast_unsigned() as u64 | if core { 0x80 } else { 0 }
            }
        }
    }

    const fn wait_status(self, tid: Tid) -> WaitStatus {
        match self {
            Self::Code(code) => WaitStatus::Exited(tid, code & 0xff),
            Self::Signal(signal, core) => WaitStatus::Signaled(tid, signal, core),
        }
    }
}

/// Why a stopped thread stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopKind {
    /// A signal-delivery-stop: the signal waits for the tracer's verdict.
    Signal(i32),
    /// The `PTRACE_EVENT_EXIT` stop of a thread ending this way.
    Exit(ExitStatus),
}

/// Where a thread is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Executing instructions whenever the world runs it.
    Running,
    /// In a ptrace-stop, answering the tracer's requests.
    Stopped { kind: StopKind, info: SigInfo },
    /// On its way out, which takes it to its exit event the next time it
    /// runs, or straight to a zombie when exits are not traced.
    Exiting(ExitStatus),
    /// Gone, but its status not yet reaped.
    Zombie(ExitStatus),
}

/// The `PTRACE_O_*` options the simulation honors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    pub trace_exit: bool,
    pub exit_kill: bool,
}

pub struct Thread {
    pub tid: Tid,
    pub tgid: Tid,
    pub registers: Registers,
    pub state: State,
    pub options: Options,
    /// Signals pending for this thread alone.
    pub pending: SignalSet,
    /// Whether the thread stops after its next instruction.
    pub single_step: bool,
    /// The status a wait would report now, until one reaps it.
    pub report: Option<WaitStatus>,
}

impl Thread {
    #[must_use]
    pub const fn is_stopped(&self) -> bool {
        matches!(self.state, State::Stopped { .. })
    }

    /// Whether running the thread can change anything.
    #[must_use]
    pub const fn can_run(&self) -> bool {
        matches!(self.state, State::Running | State::Exiting(_))
    }
}

pub struct Process {
    pub tgid: Tid,
    /// The name `comm` reports: the executable's file name, cut to fifteen
    /// bytes.
    pub name: Arc<str>,
    pub space: AddressSpace,
    pub image: Arc<Image>,
    /// What the program wrote to its standard output and error, in order.
    pub output: Vec<u8>,
    /// The process's exit status once its last thread is reaped.
    pub exit: Option<ExitStatus>,
}

/// Something the simulation does not model, which ends the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelGap(pub String);

pub struct Kernel {
    /// The process identifier of the simulated debugger, which sends the
    /// signals it asks for.
    tracer: i32,
    next_tid: Tid,
    pub processes: BTreeMap<Tid, Process>,
    pub threads: BTreeMap<Tid, Thread>,
    /// Processes whose last thread was reaped, with how they ended.
    pub ended: BTreeMap<Tid, (ExitStatus, Vec<u8>)>,
    /// The first model gap the run hit.
    pub gap: Option<ModelGap>,
    /// Makes `PTRACE_POKEDATA` report success without writing, so tests
    /// can check that the oracles notice a debugger whose writes are lost.
    #[cfg(test)]
    pub lose_pokes: bool,
}

impl Kernel {
    #[must_use]
    pub const fn new(tracer: i32) -> Self {
        Self {
            tracer,
            next_tid: FIRST_TID,
            processes: BTreeMap::new(),
            threads: BTreeMap::new(),
            ended: BTreeMap::new(),
            gap: None,
            #[cfg(test)]
            lose_pokes: false,
        }
    }

    #[must_use]
    pub const fn tracer(&self) -> i32 {
        self.tracer
    }

    /// Records that the run reached something the simulation does not
    /// model. The first gap is the one reported.
    pub fn gap(&mut self, description: impl Into<String>) {
        self.gap.get_or_insert_with(|| ModelGap(description.into()));
    }

    /// Starts `image` as a traced child that has just executed it (K-EXEC-1):
    /// it reports a stop for `SIGTRAP` from itself, at its entry point.
    pub fn spawn(
        &mut self,
        image: Arc<Image>,
        name: &str,
        arguments: &[String],
        random: [u8; 16],
    ) -> Tid {
        let tid = self.next_tid;
        self.next_tid += 1;
        let (space, registers) = image.load(arguments, random);
        let comm = name.rsplit('/').next().unwrap_or(name);
        let comm = &comm[..comm.len().min(15)];
        self.processes.insert(
            tid,
            Process {
                tgid: tid,
                name: Arc::from(comm),
                space,
                image,
                output: Vec::new(),
                exit: None,
            },
        );
        let info = SigInfo {
            signal: SIGTRAP,
            code: signals::SI_USER,
            pid: tid,
            address: 0,
        };
        self.threads.insert(
            tid,
            Thread {
                tid,
                tgid: tid,
                registers,
                state: State::Stopped {
                    kind: StopKind::Signal(SIGTRAP),
                    info,
                },
                options: Options::default(),
                pending: SignalSet::default(),
                single_step: false,
                report: Some(WaitStatus::Stopped(tid, SIGTRAP)),
            },
        );
        tid
    }

    /// Threads that can make progress if run.
    pub fn runnable(&self) -> impl Iterator<Item = Tid> + '_ {
        self.threads
            .values()
            .filter(|thread| thread.can_run())
            .map(|thread| thread.tid)
    }

    /// Threads with a status a wait would report.
    pub fn reportable(&self) -> impl Iterator<Item = Tid> + '_ {
        self.threads
            .values()
            .filter(|thread| thread.report.is_some())
            .map(|thread| thread.tid)
    }

    /// Reaps `tid`'s status, as the waiter's `waitpid` does. A zombie is
    /// released, and its process ends with its last thread.
    pub fn collect(&mut self, tid: Tid) -> Option<WaitStatus> {
        let thread = self.threads.get_mut(&tid)?;
        let status = thread.report.take()?;
        if let State::Zombie(exit) = thread.state {
            let group = thread.tgid;
            self.threads.remove(&tid);
            if !self.threads.values().any(|thread| thread.tgid == group)
                && let Some(process) = self.processes.remove(&group)
            {
                self.ended
                    .insert(group, (process.exit.unwrap_or(exit), process.output));
            }
        }
        Some(status)
    }

    /// Runs `tid` for up to `budget` instructions, stopping early when it
    /// stops, exits, or hits a gap. Returns how many instructions ran.
    pub fn run(&mut self, tid: Tid, budget: u64) -> u64 {
        let mut executed = 0;
        while executed < budget && self.gap.is_none() {
            let Some(thread) = self.threads.get(&tid) else {
                break;
            };
            match thread.state {
                State::Exiting(exit) => {
                    self.reach_exit(tid, exit);
                    break;
                }
                State::Running => {}
                State::Stopped { .. } | State::Zombie(_) => break,
            }
            if self.deliver_pending(tid) {
                break;
            }
            executed += 1;
            if self.execute(tid) {
                break;
            }
        }
        executed
    }

    /// Executes one instruction of a running thread. Returns whether the
    /// thread stopped running.
    fn execute(&mut self, tid: Tid) -> bool {
        let thread = self.threads.get_mut(&tid).expect("running thread exists");
        let process = self
            .processes
            .get_mut(&thread.tgid)
            .expect("a thread's process exists");
        let single_step = thread.single_step;
        let outcome = cpu::step(&mut thread.registers, &mut process.space);
        match outcome {
            Outcome::Completed => {}
            Outcome::Syscall => {
                syscalls::serve(self, tid);
                if !matches!(
                    self.threads.get(&tid).map(|thread| thread.state),
                    Some(State::Running)
                ) {
                    return true;
                }
                if single_step {
                    // K-TRAP-1: a step across `syscall` reports TRAP_BRKPT.
                    self.trap(tid, signals::TRAP_BRKPT);
                    return true;
                }
                return false;
            }
            Outcome::Breakpoint => {
                // K-TRAP-1: int3 reports SI_KERNEL with rip past the trap.
                self.signal_stop(
                    tid,
                    SigInfo {
                        signal: SIGTRAP,
                        code: cpu::SI_KERNEL,
                        ..SigInfo::default()
                    },
                );
                return true;
            }
            Outcome::Fault(fault) => {
                self.signal_stop(
                    tid,
                    SigInfo {
                        signal: fault.signal,
                        code: fault.code,
                        pid: 0,
                        address: fault.address.unwrap_or(0),
                    },
                );
                return true;
            }
            Outcome::Unsupported(instruction) => {
                self.gap(format!("instruction {instruction}"));
                return true;
            }
        }
        if single_step {
            // K-TRAP-1: a single step reports TRAP_TRACE.
            self.trap(tid, signals::TRAP_TRACE);
            return true;
        }
        false
    }

    /// Stops a thread for `SIGTRAP` with `code` at its current `rip`.
    fn trap(&mut self, tid: Tid, code: i32) {
        let rip = self.threads[&tid].registers.rip;
        self.signal_stop(
            tid,
            SigInfo {
                signal: SIGTRAP,
                code,
                pid: 0,
                address: rip,
            },
        );
    }

    /// Enters a signal-delivery-stop for `info`.
    fn signal_stop(&mut self, tid: Tid, info: SigInfo) {
        let thread = self.threads.get_mut(&tid).expect("stopping thread exists");
        thread.state = State::Stopped {
            kind: StopKind::Signal(info.signal),
            info,
        };
        thread.report = Some(WaitStatus::Stopped(tid, info.signal));
    }

    /// The process `tid` belongs to.
    #[must_use]
    pub fn process_of(&self, tid: Tid) -> Option<&Process> {
        self.processes.get(&self.threads.get(&tid)?.tgid)
    }

    /// `/proc/<tid>/maps`, or `None` once the thread is gone.
    #[must_use]
    pub fn maps(&self, tid: Tid) -> Option<String> {
        let thread = self.threads.get(&tid)?;
        if matches!(thread.state, State::Zombie(_)) {
            return Some(String::new());
        }
        Some(self.processes.get(&thread.tgid)?.space.maps())
    }
}
