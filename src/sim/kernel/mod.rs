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

pub mod debug_regs;
mod processes;
pub mod ptrace;
pub mod shadow;
pub mod signals;
mod syscalls;
pub mod watching;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use nix::libc;

use self::watching::Watching;
use super::cpu::{self, Outcome, RAX, Registers};
use super::loader::Image;
use super::memory::AddressSpace;
#[cfg(test)]
use super::world::Sabotage;
use debug_regs::DebugRegisters;
use shadow::{Shadow, Tracking};
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
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is one independent PTRACE_O_* flag"
)]
pub struct Options {
    pub trace_clone: bool,
    pub trace_fork: bool,
    pub trace_exit: bool,
    pub exit_kill: bool,
}

/// A byte of a process's code that differs from its program's: its
/// address, the byte, and the program's.
pub type Planted = (u64, u8, u8);

/// How the tracer traces a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tracing {
    /// Not at all: the thread runs as if no debugger were there, as one the
    /// tracer detached does (K-FORK-2).
    Untraced,
    /// Attached, as a launched program and the threads it creates are.
    Attached,
    /// Seized, or created by a seized thread: interrupts reach it, the
    /// threads and processes it creates first stop for one, and one may
    /// wait to stop it, which any stop takes the place of (K-SEIZE-1,
    /// K-INT-1).
    Seized { interrupted: bool },
}

pub struct Thread {
    pub tid: Tid,
    pub tgid: Tid,
    pub tracing: Tracing,
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
    /// Where the thread last executed a trap, and how many instructions it
    /// had completed then.
    pub last_trap: Option<(u64, u64)>,
    /// How many instructions the thread completed, `syscall` among them.
    pub retired: u64,
    /// The calls the thread made and has not returned from.
    pub shadow: Shadow,
    pub debug: DebugRegisters,
    /// The hardware breakpoints of this thread others hold, as perf can.
    pub debug_held: usize,
}

impl Thread {
    #[must_use]
    pub const fn is_stopped(&self) -> bool {
        matches!(self.state, State::Stopped { .. })
    }

    /// Whether the tracer traces the thread.
    #[must_use]
    pub const fn traced(&self) -> bool {
        !matches!(self.tracing, Tracing::Untraced)
    }

    /// Whether the tracer seized the thread, or traces it as one a seized
    /// thread created.
    #[must_use]
    pub const fn seized(&self) -> bool {
        matches!(self.tracing, Tracing::Seized { .. })
    }

    /// Whether the thread is held in a ptrace-stop it would leave only for
    /// the tracer: any but its exit event, which leads only to its end.
    #[must_use]
    pub const fn held(&self) -> bool {
        matches!(
            self.state,
            State::Stopped { kind, .. } if !matches!(kind, StopKind::Exit(_))
        )
    }

    /// Enters a ptrace-stop that reports `report`. Any stop takes the place
    /// of an interrupt still waiting (K-INT-1).
    pub(super) const fn enter_stop(&mut self, kind: StopKind, info: SigInfo, report: WaitStatus) {
        self.state = State::Stopped { kind, info };
        self.report = Some(report);
        if let Tracing::Seized { interrupted } = &mut self.tracing {
            *interrupted = false;
        }
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
    /// Who reaps the process once it ends.
    pub parent: Parent,
    /// The thread that forked it, which `/proc` lists it under, until that
    /// thread exits and another of its group takes it on.
    pub creator: Option<Tid>,
    /// The process the tracer launched whose standard output and error
    /// this one shares, its own included.
    pub root: Tid,
    /// Signals pending for the whole process, which whichever of its
    /// threads looks first takes.
    pub shared: Pending,
    /// The status of the group exit: from `exit_group`, a fatal signal, or
    /// the thread that began to exit last (K-EXIT-6).
    pub group_exit: Option<ExitStatus>,
    /// Whether something outside the session killed the process.
    pub killed_externally: bool,
}

/// Who reaps a process once it ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parent {
    /// The tracer, which launched it.
    Tracer,
    /// Whoever started it untraced, for the tracer to attach to later,
    /// which reaps it at once.
    Launcher,
    /// The process that forked it, with `wait4`, after SIGCHLD.
    Process(Tid),
    /// The reaper orphans pass to, which reaps them at once (K-FORK-3).
    Init,
}

impl fmt::Display for Parent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tracer => formatter.write_str("the tracer"),
            Self::Launcher => formatter.write_str("its launcher"),
            Self::Process(tgid) => write!(formatter, "process {tgid}"),
            Self::Init => formatter.write_str("init"),
        }
    }
}

/// How a process ended, and who reaped it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ended {
    pub status: ExitStatus,
    /// The process the tracer launched whose output this one shared.
    pub root: Tid,
    pub reaper: Parent,
    /// Whether something outside the session killed it.
    pub killed_externally: bool,
}

/// A process that ended, waiting for its parent to reap it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Zombie {
    pub parent: Tid,
    pub creator: Option<Tid>,
    pub status: ExitStatus,
    pub root: Tid,
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
    /// A thread forked a process.
    Forked { parent: Tid, child: Tid },
    /// The tracer detached a thread, leaving the first byte of its code
    /// that differs from the program's, as its address, the byte, and the
    /// program's.
    Released { tid: Tid, planted: Option<Planted> },
    /// A process reaped a child it forked.
    ReapedChild { parent: Tid, child: Tid },
    /// Init reaped a process whose parent was gone.
    ReapedOrphan { tgid: Tid },
    /// The tracer tried to seize a thread that had finished exiting.
    SeizeRefused { tid: Tid },
    /// An interrupt reached a thread already stopped, which it stops again
    /// as soon as it resumes.
    InterruptWaits { tid: Tid },
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
    /// Processes reaped, with how they ended.
    pub ended: BTreeMap<Tid, Ended>,
    /// Processes that ended and wait for their parents to reap them.
    pub zombies: BTreeMap<Tid, Zombie>,
    /// What each process the tracer launched, and the processes it forked,
    /// wrote to standard output and error, in order, by launched process.
    pub outputs: BTreeMap<Tid, Vec<u8>>,
    /// The status each reaped thread reported.
    pub reaped: BTreeMap<Tid, WaitStatus>,
    /// The first model gap the run hit.
    pub gap: Option<ModelGap>,
    /// What happened since the world last looked.
    pub happenings: Vec<Happening>,
    /// The process the debugger controls, whose memory the user's
    /// breakpoints and watches are in.
    pub debugged: Option<Tid>,
    /// Addresses where the user's breakpoints are certainly enabled, as
    /// the client last knew them. The breakpoint-accounting oracle watches
    /// executions there.
    pub user_breakpoints: BTreeSet<u64>,
    /// Executions at those addresses no trap reported.
    pub unseen_hits: Vec<UnseenHit>,
    /// Where the thread the client steps has been, while it steps.
    pub tracking: Option<Tracking>,
    /// What the kernel follows of the client's watchpoints.
    pub watching: Watching,
    /// How the debug registers answer the tracer.
    pub debug_behavior: DebugBehavior,
    /// The identifier the next activation takes.
    next_activation: u64,
    /// A deliberate defect, so tests can check that the oracles notice.
    #[cfg(test)]
    pub sabotage: Option<Sabotage>,
    /// The thread whose single step went on past one instruction, under
    /// [`Sabotage::LateSingleSteps`].
    #[cfg(test)]
    late_step: Option<Tid>,
    /// The debug registers the tracer believes it wrote, under
    /// [`Sabotage::PhantomArming`].
    #[cfg(test)]
    pub(super) phantom_debug: BTreeMap<Tid, debug_regs::DebugRegisters>,
}

/// How a thread's debug registers answer the tracer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugBehavior {
    /// As Linux's do (K-DR-*).
    Faithful,
    /// Writes succeed and change nothing, and reads give zero, as under
    /// gVisor.
    Discarding,
    /// Others hold this many hardware breakpoints of every thread the
    /// program creates, as a perf session following new threads can, so
    /// fewer of their slots take an address (K-DR-4).
    Contended(usize),
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
            zombies: BTreeMap::new(),
            outputs: BTreeMap::new(),
            reaped: BTreeMap::new(),
            gap: None,
            happenings: Vec::new(),
            debugged: None,
            user_breakpoints: BTreeSet::new(),
            unseen_hits: Vec::new(),
            tracking: None,
            watching: Watching::new(),
            debug_behavior: DebugBehavior::Faithful,
            next_activation: 1,
            #[cfg(test)]
            sabotage: None,
            #[cfg(test)]
            phantom_debug: BTreeMap::new(),
            #[cfg(test)]
            late_step: None,
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

    /// The shadow of a thread that has made no call yet.
    const fn new_shadow(&mut self) -> Shadow {
        let base = self.next_activation;
        self.next_activation += 1;
        Shadow {
            base,
            calls: Vec::new(),
            lost: false,
        }
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
        self.start(image, name, arguments, random, true)
    }

    /// Starts `image` untraced, running from its entry point, for the
    /// tracer to attach to.
    pub fn spawn_untraced(
        &mut self,
        image: Arc<Image>,
        name: &str,
        arguments: &[String],
        random: [u8; 16],
    ) -> Tid {
        self.start(image, name, arguments, random, false)
    }

    fn start(
        &mut self,
        image: Arc<Image>,
        name: &str,
        arguments: &[String],
        random: [u8; 16],
        traced: bool,
    ) -> Tid {
        let tid = self.allocate_tid();
        let shadow = self.new_shadow();
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
                parent: if traced {
                    Parent::Tracer
                } else {
                    Parent::Launcher
                },
                creator: None,
                root: tid,
                shared: Pending::default(),
                group_exit: None,
                killed_externally: false,
            },
        );
        self.outputs.insert(tid, Vec::new());
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
                tracing: if traced {
                    Tracing::Attached
                } else {
                    Tracing::Untraced
                },
                registers,
                // The stop is at the end of `execve`.
                orig_rax: syscalls::SYS_EXECVE,
                returning: None,
                state: if traced {
                    State::Stopped {
                        kind: StopKind::Signal(SIGTRAP),
                        info,
                    }
                } else {
                    State::Running
                },
                options: Options::default(),
                pending: Pending::default(),
                single_step: false,
                report: traced.then_some(WaitStatus::Stopped(tid, SIGTRAP)),
                trapped_at: None,
                last_trap: None,
                retired: 0,
                shadow,
                debug: DebugRegisters::default(),
                debug_held: 0,
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
                self.end_process(group, exit);
            } else {
                self.end_with_untraced_leader(group);
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
            if self.stop_for_interrupt(tid) || self.deliver_pending(tid) {
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
        let thread = &self.threads[&tid];
        // What a thread does is the debugger's to report only while it
        // traces the thread, in the process it debugs.
        let debugged = thread.traced() && self.debugged == Some(thread.tgid);
        if debugged {
            self.watching.resume(tid);
        }
        let thread = self.threads.get_mut(&tid).expect("running thread exists");
        let process = self
            .processes
            .get_mut(&thread.tgid)
            .expect("a thread's process exists");
        let address = thread.registers.rip;
        let trap = process.space.peek_bytes(address, 1).as_deref() == Some(&[0xcc]);
        let user_breakpoint = debugged && self.user_breakpoints.contains(&address);
        #[cfg(test)]
        let skipped = trap && self.sabotage == Some(Sabotage::SkipTraps) && user_breakpoint;
        #[cfg(not(test))]
        let skipped = false;
        if user_breakpoint && thread.trapped_at != Some(address) && (!trap || skipped) {
            self.unseen_hits.push(UnseenHit { tid, address });
        }
        thread.trapped_at = None;
        let single_step = thread.single_step;
        let before = self.watching.bytes(debugged, &process.space);
        #[cfg(test)]
        let cpu::Executed {
            outcome,
            flow,
            accesses,
        } = if skipped {
            step_under_trap(&mut thread.registers, &mut process.space, &process.image)
        } else {
            cpu::execute(&mut thread.registers, &mut process.space)
        };
        #[cfg(not(test))]
        let cpu::Executed {
            outcome,
            flow,
            accesses,
        } = cpu::execute(&mut thread.registers, &mut process.space);
        // K-DR-1: an access an enabled slot covers raises a debug
        // exception once the instruction completes.
        let watch_hits = if outcome == Outcome::Completed {
            thread.debug.hits(&accesses)
        } else {
            0
        };
        #[cfg(test)]
        let watch_hits = if self.sabotage == Some(Sabotage::MissWatchTraps) && tid != thread.tgid {
            0
        } else {
            watch_hits
        };
        if thread.debug.breaks_on_execution() {
            self.gap("debug registers breaking on execution");
            return Executed::Stopped;
        }
        let after = if accesses.iter().any(|access| access.write) {
            self.watching.bytes(debugged, &process.space)
        } else {
            before.clone()
        };
        self.watching
            .follow(tid, &thread.debug, &accesses, &before, &after);
        if matches!(outcome, Outcome::Completed | Outcome::Syscall) {
            if let Some(tracking) = &mut self.tracking {
                tracking.note(tid, &thread.shadow, address);
            }
            thread.retired += 1;
            thread
                .shadow
                .follow(flow, thread.registers.rip, &mut self.next_activation);
        }
        if outcome != Outcome::Syscall {
            // Instructions and exceptions enter the kernel, if at all, outside
            // any system call.
            thread.orig_rax = NO_SYSTEM_CALL;
        }
        match outcome {
            Outcome::Completed => self.complete(tid, single_step, watch_hits),
            Outcome::Syscall => {
                thread.orig_rax = thread.registers.general[RAX];
                self.serve_syscall(tid, single_step)
            }
            Outcome::Breakpoint | Outcome::Fault(_) | Outcome::Unsupported(_) => {
                self.take_exception(tid, address, outcome)
            }
        }
    }

    /// Serves the system call a thread just entered.
    fn serve_syscall(&mut self, tid: Tid, single_step: bool) -> Executed {
        let yielded = syscalls::serve(self, tid);
        // An untraced thread that exited is reaped at once.
        if !self
            .threads
            .get(&tid)
            .is_some_and(|thread| matches!(thread.state, State::Running))
        {
            return Executed::Stopped;
        }
        if single_step {
            // A yield gives up the CPU even under a single step.
            if yielded {
                self.happenings.push(Happening::Yielded { tid });
            }
            // K-TRAP-1: a step across `syscall` reports TRAP_BRKPT.
            self.trap(tid, signals::TRAP_BRKPT);
            return Executed::Stopped;
        }
        if yielded {
            Executed::Yielded
        } else {
            Executed::Continue
        }
    }

    /// Stops a thread for the exception the instruction at `address`
    /// raised.
    fn take_exception(&mut self, tid: Tid, address: u64, outcome: Outcome) -> Executed {
        match outcome {
            Outcome::Breakpoint => {
                // K-TRAP-1: int3 reports SI_KERNEL with rip past the trap.
                let thread = self
                    .threads
                    .get_mut(&tid)
                    .expect("the trapping thread exists");
                thread.trapped_at = Some(address);
                thread.last_trap = Some((address, thread.retired));
                let info = SigInfo {
                    signal: SIGTRAP,
                    code: cpu::SI_KERNEL,
                    ..SigInfo::default()
                };
                self.signal_stop(tid, info);
            }
            Outcome::Fault(fault) => {
                let info = SigInfo {
                    signal: fault.signal,
                    code: fault.code,
                    pid: 0,
                    address: fault.address.unwrap_or(0),
                };
                self.signal_stop(tid, info);
            }
            Outcome::Unsupported(instruction) => {
                self.gap(format!("instruction {instruction}"));
            }
            Outcome::Completed | Outcome::Syscall => {
                unreachable!("{outcome:?} raises no exception")
            }
        }
        Executed::Stopped
    }

    /// Ends an instruction that completed: a single step or a watched
    /// access raises a debug exception, and otherwise the thread runs on.
    fn complete(&mut self, tid: Tid, single_step: bool, watch_hits: u64) -> Executed {
        if single_step {
            #[cfg(test)]
            if watch_hits == 0 && self.step_late(tid) {
                return Executed::Continue;
            }
            // K-TRAP-1: a single step reports TRAP_TRACE. K-DR-2: with DR6
            // holding the step and any slot that hit.
            self.threads
                .get_mut(&tid)
                .expect("the stepping thread exists")
                .debug
                .debug_exception(true, watch_hits);
            self.trap(tid, signals::TRAP_TRACE);
            return Executed::Stopped;
        }
        if watch_hits != 0 {
            // K-DR-1: TRAP_HWBKPT with `rip` after the instruction.
            self.threads
                .get_mut(&tid)
                .expect("the accessing thread exists")
                .debug
                .debug_exception(false, watch_hits);
            self.trap(tid, signals::TRAP_HWBKPT);
            return Executed::Stopped;
        }
        Executed::Continue
    }

    /// Whether a single step of the thread the client steps goes on to a
    /// second instruction, under [`Sabotage::LateSingleSteps`].
    #[cfg(test)]
    fn step_late(&mut self, tid: Tid) -> bool {
        let stepped = self
            .tracking
            .as_ref()
            .is_some_and(|tracking| tracking.tid == tid);
        if self.sabotage != Some(Sabotage::LateSingleSteps) || !stepped {
            return false;
        }
        self.late_step.take() != Some(tid) && {
            self.late_step = Some(tid);
            true
        }
    }

    /// A word ptrace reads, as a sabotaged kernel reports it.
    #[cfg(test)]
    pub(super) fn sabotage_read(&self, tgid: Tid, address: u64, word: u64) -> u64 {
        let return_slot = |exact: bool| {
            self.threads_of(tgid).any(|thread| {
                thread
                    .shadow
                    .calls
                    .iter()
                    .any(|call| call.slot == address && (!exact || call.return_address == word))
            })
        };
        match self.sabotage {
            Some(Sabotage::SkewReturnAddresses) if return_slot(true) => word + 1,
            Some(Sabotage::SkewSmallStackWords)
                if (1..0x1000).contains(&word)
                    && !return_slot(false)
                    && self.processes[&tgid].space.maps().lines().any(|line| {
                        line.ends_with("[stack]") && {
                            let range = line.split_whitespace().next().unwrap_or_default();
                            let (start, end) = range.split_once('-').unwrap_or_default();
                            (u64::from_str_radix(start, 16).unwrap_or(0)
                                ..u64::from_str_radix(end, 16).unwrap_or(0))
                                .contains(&address)
                        }
                    }) =>
            {
                word + 1
            }
            _ => word,
        }
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

    /// Enters a signal-delivery-stop for `info`, or, for an untraced
    /// thread, takes the signal's default action.
    fn signal_stop(&mut self, tid: Tid, info: SigInfo) {
        let thread = self.threads.get_mut(&tid).expect("stopping thread exists");
        if !thread.traced() {
            self.act_by_default(tid, info);
            return;
        }
        thread.enter_stop(
            StopKind::Signal(info.signal),
            info,
            WaitStatus::Stopped(tid, info.signal),
        );
    }

    /// Stops a thread inside the system call it is making for a
    /// `PTRACE_EVENT_*` stop. The call returns `result` once the thread
    /// resumes.
    fn event_stop(&mut self, tid: Tid, event: i32, message: u64, result: u64) {
        let thread = self.threads.get_mut(&tid).expect("stopping thread exists");
        thread.registers.general[RAX] = ENOSYS_RESULT;
        thread.returning = Some(result);
        thread.enter_stop(
            StopKind::Event(event, message),
            SigInfo::event(tid, event),
            WaitStatus::Event(tid, event),
        );
    }

    /// Stops a running thread for the interrupt waiting for it, with
    /// `PTRACE_EVENT_STOP`, before it takes a signal or runs (K-INT-1).
    /// Returns whether it stopped.
    fn stop_for_interrupt(&mut self, tid: Tid) -> bool {
        let thread = self.threads.get_mut(&tid).expect("running thread exists");
        if thread.tracing != (Tracing::Seized { interrupted: true }) {
            return false;
        }
        thread.enter_stop(
            StopKind::Event(libc::PTRACE_EVENT_STOP, 0),
            SigInfo::event(tid, libc::PTRACE_EVENT_STOP),
            WaitStatus::Event(tid, libc::PTRACE_EVENT_STOP),
        );
        true
    }

    /// The first byte of `group`'s code that differs from its program's:
    /// its address, the byte, and the program's. A page still shared with
    /// the image was never written.
    #[must_use]
    pub fn planted(&self, group: Tid) -> Option<Planted> {
        let process = self.processes.get(&group)?;
        process.image.code().find_map(|(page_address, original)| {
            if process
                .space
                .page(page_address)
                .is_some_and(|page| Arc::ptr_eq(page, original))
            {
                return None;
            }
            let current = process.space.peek_bytes(page_address, original.len())?;
            current
                .iter()
                .zip(original.iter())
                .enumerate()
                .find(|(_, (now, was))| now != was)
                .map(|(offset, (&now, &was))| (page_address + offset as u64, now, was))
        })
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
/// it, as a broken CPU would, for [`Sabotage::SkipTraps`].
#[cfg(test)]
fn step_under_trap(
    registers: &mut Registers,
    space: &mut AddressSpace,
    image: &Image,
) -> cpu::Executed {
    let address = registers.rip;
    let original = image
        .original_byte(address)
        .expect("a trap in the program's code");
    assert!(space.poke_bytes(address, &[original]));
    let executed = cpu::execute(registers, space);
    assert!(space.poke_bytes(address, &[0xcc]));
    executed
}

/// What executing one instruction did to the thread's run.
enum Executed {
    Continue,
    Stopped,
    Yielded,
}
