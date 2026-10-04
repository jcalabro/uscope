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

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use nix::libc;

use super::cpu::{self, Outcome, RAX, Registers};
use super::loader::Image;
use super::memory::AddressSpace;
use signals::{Pending, SIGTRAP};

/// A thread or process identifier.
pub type Tid = i32;

/// The identifier the first simulated process gets.
const FIRST_TID: Tid = 1000;
/// The user every simulated process runs as.
const UID: u32 = 1000;
/// `orig_rax` outside any system call.
pub const NO_SYSTEM_CALL: u64 = u64::MAX;
/// What `rax` holds while a thread is inside a system call.
const ENOSYS_RESULT: u64 = (-(libc::ENOSYS as i64)).cast_unsigned();

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

impl SigInfo {
    /// The siginfo of `tid`'s `PTRACE_EVENT_*` stop: SIGTRAP with the
    /// event in the code's second byte, sent by the thread itself.
    const fn event(tid: Tid, event: i32) -> Self {
        Self {
            signal: SIGTRAP,
            code: SIGTRAP | (event << 8),
            pid: tid,
            // `si_pid` and `si_uid` overlay `si_addr`.
            address: (UID as u64) << 32 | tid.cast_unsigned() as u64,
        }
    }
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
    /// A `PTRACE_EVENT_*` stop other than the exit event, with the message
    /// `PTRACE_GETEVENTMSG` reports.
    Event(i32, u64),
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
    /// Gone, having ended this way, but not yet reaped.
    Zombie(ExitStatus),
}

impl State {
    /// Whether the thread has begun to exit.
    #[must_use]
    pub const fn exiting(self) -> bool {
        matches!(
            self,
            Self::Exiting(_)
                | Self::Zombie(_)
                | Self::Stopped {
                    kind: StopKind::Exit(_),
                    ..
                }
        )
    }
}

/// The `PTRACE_O_*` options the simulation honors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    pub trace_clone: bool,
    pub trace_exit: bool,
    pub exit_kill: bool,
}

pub struct Thread {
    pub tid: Tid,
    pub tgid: Tid,
    pub registers: Registers,
    /// `orig_rax`: the system call the thread entered last, until it runs
    /// another instruction or takes an exception, or [`NO_SYSTEM_CALL`].
    pub orig_rax: u64,
    /// The result of a system call the thread stopped inside. `rax` holds
    /// `-ENOSYS` until the call returns it.
    pub returning: Option<u64>,
    pub state: State,
    pub options: Options,
    /// Signals pending for this thread alone.
    pub pending: Pending,
    /// Whether the thread stops after its next instruction.
    pub single_step: bool,
    /// The status a wait would report now, until one reaps it.
    pub report: Option<WaitStatus>,
    /// The address of the trap the thread executed, until it executes
    /// another instruction.
    pub trapped_at: Option<u64>,
    /// How many traps the thread executed.
    pub traps: u64,
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
    /// The status of the group exit: from `exit_group`, a fatal signal, or
    /// the thread that began to exit last (K-EXIT-6).
    pub group_exit: Option<ExitStatus>,
    /// Whether something outside the session killed the process.
    pub killed_externally: bool,
}

/// Something the simulation does not model, which ends the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelGap(pub String);

/// Something that happened in the kernel which the world counts or
/// reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Happening {
    /// A thread created another.
    Cloned { parent: Tid, child: Tid },
    /// A thread called `exit_group`.
    GroupExit { tid: Tid },
    /// A group exit or SIGKILL took a thread out of a ptrace-stop.
    PulledFromStop { tid: Tid },
    /// A group leader exited while other threads ran on.
    LeaderExitedAlone { tid: Tid },
    /// A thread yielded the CPU.
    Yielded { tid: Tid },
}

/// A thread that executed the program's own instruction at an address
/// where the user's breakpoint is enabled, without having just trapped
/// there: a hit the debugger could not see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnseenHit {
    pub tid: Tid,
    pub address: u64,
}

pub struct Kernel {
    /// The process identifier of the simulated debugger, which sends the
    /// signals it asks for.
    tracer: i32,
    next_tid: Tid,
    pub processes: BTreeMap<Tid, Process>,
    pub threads: BTreeMap<Tid, Thread>,
    /// Processes whose last thread was reaped, with how they ended.
    pub ended: BTreeMap<Tid, (ExitStatus, Vec<u8>)>,
    /// The status each reaped thread reported.
    pub reaped: BTreeMap<Tid, WaitStatus>,
    /// The first model gap the run hit.
    pub gap: Option<ModelGap>,
    /// What happened since the world last looked.
    pub happenings: Vec<Happening>,
    /// Addresses where the user's breakpoints are certainly enabled, as
    /// the client last knew them. The breakpoint-accounting oracle watches
    /// executions there.
    pub user_breakpoints: BTreeSet<u64>,
    /// Executions at those addresses no trap reported.
    pub unseen_hits: Vec<UnseenHit>,
    /// Makes `PTRACE_POKEDATA` report success without writing, so tests
    /// can check that the oracles notice a debugger whose writes are lost.
    #[cfg(test)]
    pub lose_pokes: bool,
    /// Makes the CPU execute the program's own instruction under a trap at
    /// a user breakpoint, so tests can check that the oracles notice.
    #[cfg(test)]
    pub skip_traps: bool,
}

/// What running a thread for a while did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ran {
    /// How many instructions it executed.
    pub executed: u64,
    /// Whether it gave up the CPU with `sched_yield`.
    pub yielded: bool,
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
            reaped: BTreeMap::new(),
            gap: None,
            happenings: Vec::new(),
            user_breakpoints: BTreeSet::new(),
            unseen_hits: Vec::new(),
            #[cfg(test)]
            lose_pokes: false,
            #[cfg(test)]
            skip_traps: false,
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

    const fn allocate_tid(&mut self) -> Tid {
        let tid = self.next_tid;
        self.next_tid += 1;
        tid
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
        let tid = self.allocate_tid();
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
                group_exit: None,
                killed_externally: false,
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
                // The stop is at the end of `execve`.
                orig_rax: syscalls::SYS_EXECVE,
                returning: None,
                state: State::Stopped {
                    kind: StopKind::Signal(SIGTRAP),
                    info,
                },
                options: Options::default(),
                pending: Pending::default(),
                single_step: false,
                report: Some(WaitStatus::Stopped(tid, SIGTRAP)),
                trapped_at: None,
                traps: 0,
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

    /// Threads with a status a wait would report now. A group leader's
    /// exit waits until every other thread of its group is reaped (K-WAIT-2).
    pub fn reportable(&self) -> impl Iterator<Item = Tid> + '_ {
        self.threads
            .values()
            .filter(|thread| thread.report.is_some() && !self.delayed(thread))
            .map(|thread| thread.tid)
    }

    /// Whether `thread` is a zombie leader whose group still has others.
    fn delayed(&self, thread: &Thread) -> bool {
        thread.tid == thread.tgid
            && matches!(thread.state, State::Zombie(_))
            && self
                .threads
                .values()
                .any(|other| other.tgid == thread.tgid && other.tid != thread.tid)
    }

    /// Reaps `tid`'s status, as the waiter's `waitpid` does. A zombie is
    /// released, and its process ends with its last thread.
    pub fn collect(&mut self, tid: Tid) -> Option<WaitStatus> {
        let thread = self.threads.get(&tid)?;
        if thread.report.is_none() || self.delayed(thread) {
            return None;
        }
        let thread = self.threads.get_mut(&tid).expect("the thread exists");
        let mut status = thread.report.take().expect("a report");
        if let State::Zombie(own) = thread.state {
            let group = thread.tgid;
            let leader = tid == group;
            self.threads.remove(&tid);
            let process = self.processes.get(&group).expect("a zombie's process");
            // K-EXIT-6: a group exit decides every status reaped after it.
            let exit = process.group_exit.unwrap_or(own);
            status = exit.wait_status(tid);
            self.reaped.insert(tid, status);
            if leader {
                let process = self.processes.remove(&group).expect("the process");
                self.ended.insert(group, (exit, process.output));
            }
        }
        Some(status)
    }

    /// Runs `tid` for up to `budget` instructions, stopping early when it
    /// stops, exits, yields, or hits a gap.
    pub fn run(&mut self, tid: Tid, budget: u64) -> Ran {
        let mut ran = Ran {
            executed: 0,
            yielded: false,
        };
        while ran.executed < budget && self.gap.is_none() {
            let Some(thread) = self.threads.get_mut(&tid) else {
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
            if let Some(result) = thread.returning.take() {
                // The system call the thread stopped inside returns.
                thread.registers.general[RAX] = result;
                if thread.single_step {
                    // K-TRAP-1: a step across `syscall` reports TRAP_BRKPT.
                    self.trap(tid, signals::TRAP_BRKPT);
                    break;
                }
            }
            if self.deliver_pending(tid) {
                break;
            }
            ran.executed += 1;
            match self.execute(tid) {
                Executed::Continue => {}
                Executed::Stopped => break,
                Executed::Yielded => {
                    ran.yielded = true;
                    self.happenings.push(Happening::Yielded { tid });
                    break;
                }
            }
        }
        ran
    }

    /// Executes one instruction of a running thread.
    fn execute(&mut self, tid: Tid) -> Executed {
        let thread = self.threads.get_mut(&tid).expect("running thread exists");
        let process = self
            .processes
            .get_mut(&thread.tgid)
            .expect("a thread's process exists");
        let address = thread.registers.rip;
        let trap = process.space.peek_bytes(address, 1).as_deref() == Some(&[0xcc]);
        #[cfg(test)]
        let skipped = trap && self.skip_traps && self.user_breakpoints.contains(&address);
        #[cfg(not(test))]
        let skipped = false;
        if self.user_breakpoints.contains(&address)
            && thread.trapped_at != Some(address)
            && (!trap || skipped)
        {
            self.unseen_hits.push(UnseenHit { tid, address });
        }
        thread.trapped_at = None;
        let single_step = thread.single_step;
        #[cfg(test)]
        let outcome = if skipped {
            step_under_trap(&mut thread.registers, &mut process.space, &process.image)
        } else {
            cpu::step(&mut thread.registers, &mut process.space)
        };
        #[cfg(not(test))]
        let outcome = cpu::step(&mut thread.registers, &mut process.space);
        if outcome != Outcome::Syscall {
            // Instructions and exceptions enter the kernel, if at all, outside
            // any system call.
            thread.orig_rax = NO_SYSTEM_CALL;
        }
        match outcome {
            Outcome::Completed => {}
            Outcome::Syscall => {
                thread.orig_rax = thread.registers.general[RAX];
                let yielded = syscalls::serve(self, tid);
                let thread = &self.threads[&tid];
                if !matches!(thread.state, State::Running) {
                    return Executed::Stopped;
                }
                if single_step {
                    // K-TRAP-1: a step across `syscall` reports TRAP_BRKPT.
                    self.trap(tid, signals::TRAP_BRKPT);
                    return Executed::Stopped;
                }
                return if yielded {
                    Executed::Yielded
                } else {
                    Executed::Continue
                };
            }
            Outcome::Breakpoint => {
                // K-TRAP-1: int3 reports SI_KERNEL with rip past the trap.
                thread.trapped_at = Some(address);
                thread.traps += 1;
                self.signal_stop(
                    tid,
                    SigInfo {
                        signal: SIGTRAP,
                        code: cpu::SI_KERNEL,
                        ..SigInfo::default()
                    },
                );
                return Executed::Stopped;
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
                return Executed::Stopped;
            }
            Outcome::Unsupported(instruction) => {
                self.gap(format!("instruction {instruction}"));
                return Executed::Stopped;
            }
        }
        if single_step {
            // K-TRAP-1: a single step reports TRAP_TRACE.
            self.trap(tid, signals::TRAP_TRACE);
            return Executed::Stopped;
        }
        Executed::Continue
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

    /// Stops a thread inside the system call it is making for a
    /// `PTRACE_EVENT_*` stop. The call returns `result` once the thread
    /// resumes.
    fn event_stop(&mut self, tid: Tid, event: i32, message: u64, result: u64) {
        let thread = self.threads.get_mut(&tid).expect("stopping thread exists");
        thread.registers.general[RAX] = ENOSYS_RESULT;
        thread.returning = Some(result);
        thread.state = State::Stopped {
            kind: StopKind::Event(event, message),
            info: SigInfo::event(tid, event),
        };
        thread.report = Some(WaitStatus::Event(tid, event));
    }

    /// The process `tid` belongs to.
    #[must_use]
    pub fn process_of(&self, tid: Tid) -> Option<&Process> {
        self.processes.get(&self.threads.get(&tid)?.tgid)
    }

    /// `/proc/<tid>/maps`, or `None` once the thread is reaped. A zombie
    /// has no address space left, and its maps read empty.
    #[must_use]
    pub fn maps(&self, tid: Tid) -> Option<String> {
        let thread = self.threads.get(&tid)?;
        if matches!(thread.state, State::Zombie(_)) {
            return Some(String::new());
        }
        Some(self.processes.get(&thread.tgid)?.space.maps())
    }

    /// The threads of `group` not yet reaped, as `/proc/<tgid>/task` lists
    /// them.
    pub fn threads_of(&self, group: Tid) -> impl Iterator<Item = &Thread> + '_ {
        self.threads
            .values()
            .filter(move |thread| thread.tgid == group)
    }
}

/// Executes the program's own instruction at `rip` though a trap covers
/// it, as a broken CPU would, for [`Kernel::skip_traps`].
#[cfg(test)]
fn step_under_trap(registers: &mut Registers, space: &mut AddressSpace, image: &Image) -> Outcome {
    let address = registers.rip;
    let original = image
        .code()
        .find_map(|(start, page)| {
            let offset = usize::try_from(address.checked_sub(start)?).ok()?;
            page.get(offset).copied()
        })
        .expect("a trap in the program's code");
    assert!(space.poke_bytes(address, &[original]));
    let outcome = cpu::step(registers, space);
    assert!(space.poke_bytes(address, &[0xcc]));
    outcome
}

/// What executing one instruction did to the thread's run.
enum Executed {
    Continue,
    Stopped,
    Yielded,
}
