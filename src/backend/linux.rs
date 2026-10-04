//! The Linux x86-64 backend.
//!
//! One [`Controller`] per session owns all debugger state and runs on the
//! thread that created the tracee, which every ptrace call requires. A waiter
//! thread reports `waitpid` statuses back to it as messages. This module holds
//! the shared state and request dispatch; the submodules own one concern each:
//!
//! - [`native`]: the ptrace, waitpid, and `/proc` edge, behind traits that
//!   unit tests fake.
//! - [`lifecycle`]: launch, attach, thread and fork tracking, exec, shutdown.
//! - [`classify`]: turning raw wait statuses into classified stops.
//! - [`run_control`]: resuming threads, repairing breakpoint sites, and
//!   publishing all-stop snapshots.
//! - [`internal_stops`]: stopping every thread without publishing a stop,
//!   to edit breakpoints and watchpoints or repair a site while running.
//! - [`stepping`]: source and instruction stepping plans.
//! - [`breakpoints`] and [`watchpoints`]: software traps and debug registers.
//! - [`frames`], [`inspection`], [`memory`], [`registers`]: read-only views of
//!   a stopped snapshot.
//! - [`modules`]: the registry of mapped shared objects.
//! - [`post_mortem`]: serving the same views from a core dump.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};

use nix::errno::Errno;
use nix::libc;
use nix::unistd::Pid;
pub use signals::Signal;
use signals::{SignalPolicies, WaitEvent};
use tokio::sync::mpsc;

use crate::debug_info::{DebugInfo, UnwindInfo, VariableInfo};
use crate::protocol::{
    Breakpoint, BreakpointId, DebuggerEvent, ExceptionInfo, ExecutionId, ExitStatus,
    FramePresentation, FrameScopeEvidence, InferiorState, ProcessId, Reply, Request, ResumeScope,
    StateSnapshot, StepKind, StopId, StopReason, ThreadSnapshot,
    ThreadState as ObservableThreadState, WatchAccess, Watchpoint, WatchpointCapabilities,
    WatchpointId,
};
use crate::{
    CodeInstanceId, Error, GlobalVariableReference, ImageAddress, LoadedModule, ModuleImage,
    Result, SourceLocation, StackFrameId, ThreadId as DebugThreadId, UnwindTermination,
    VirtualAddress,
};

use super::{ControllerChannels, ControllerMessage, EventSender, ExecutableSource, FileIdentity};
use classify::{is_stopping_signal, is_superseded};
use debug_registers::DebugRegisterPlan;
use memory::MemoryAccessError;
use modules::{ModuleMapping, loader_link_maps, mapped_module_load_bias, module_mappings};
use native::{InspectionOps, LinuxPtrace, LinuxTraceOps, is_vanished_tracee};
use registers::Fxsave;

mod breakpoints;
mod classify;
mod core_dump;
mod core_files;
mod debug_registers;
mod disassembly;
mod frames;
mod glibc_tls;
mod inspection;
mod internal_stops;
mod libraries;
mod lifecycle;
mod memory;
mod modules;
mod native;
mod post_mortem;
#[cfg(debug_assertions)]
mod recorded;
mod registers;
mod run_control;
mod signals;
mod stepping;
mod thread_db;
mod watchpoints;
mod writes;

pub use post_mortem::{PostMortemSession, open_core};

/// Makes TLS lookups in this process use glibc's layout descriptors instead
/// of `libthread_db`.
pub fn force_internal_tls_lookup(forced: bool) {
    glibc_tls::force(forced);
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_core_dump(data: &[u8]) {
    core_dump::fuzz(data);
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_debug_register_plan(data: &[u8]) {
    debug_registers::fuzz(data);
}

/// x86-64 debug registers report writes or any access, but never reads alone.
pub fn watchpoint_capabilities() -> WatchpointCapabilities {
    WatchpointCapabilities {
        slots: u32::try_from(debug_registers::SLOT_COUNT).expect("slot count fits u32"),
        max_slot_bytes: debug_registers::MAX_SLOT_BYTES,
        access: Arc::from([
            WatchAccess::Change,
            WatchAccess::Write,
            WatchAccess::ReadWrite,
        ]),
    }
}

const CONTROLLER_THREAD_NAME: &str = "uscope-controller";
const WAITER_THREAD_NAME: &str = "uscope-waitpid";
const BREAKPOINT_OPCODE: u8 = 0xcc;
const TRAP_UNKNOWN: i32 = 5;
const TRAP_HARDWARE_BREAKPOINT: i32 = 4;
const MAX_LOGICAL_MEMORY_READ: usize = 1024 * 1024;
const MAX_PUBLIC_MEMORY_READ: u64 = 64 * 1024;
const MAX_VALUE_EXPRESSION_STEPS: usize = 64;
const MAX_VALUE_EXPRESSION_DEREFERENCES: usize = 63;
const MAX_VALUE_CHILD_PAGE_LIMIT: u32 = 256;

static LINUX_SESSION_ACTIVE: AtomicBool = AtomicBool::new(false);

/// A decoded `waitpid` status the waiter thread reports to the controller.
pub type NativeWait = WaitEvent;

fn backend_error(error: LinuxError) -> Error {
    Error::backend(error)
}

fn process_id(pid: Pid) -> ProcessId {
    ProcessId::new(u64::from(pid.as_raw().unsigned_abs()))
}

fn debug_thread_id(pid: Pid) -> DebugThreadId {
    DebugThreadId::new(u64::from(pid.as_raw().unsigned_abs()))
}

/// Converts a client-supplied thread identifier to a Linux TID.
fn debug_pid(thread: DebugThreadId) -> Result<Pid> {
    i32::try_from(thread.get())
        .ok()
        .filter(|tid| *tid > 0)
        .map(Pid::from_raw)
        .ok_or(Error::UnknownThread(thread))
}

fn exception_info(signal: Signal) -> ExceptionInfo {
    ExceptionInfo::new(signal.code(), signal.to_string())
}

fn pending_exception_info(pending: PendingSignal) -> ExceptionInfo {
    ExceptionInfo::new(
        pending.signal.code(),
        signals::describe(
            &pending.signal.name(),
            pending.signal.number(),
            pending.code,
            pending.fault_address,
            pending.sender,
        ),
    )
}

struct SessionLease {
    owns_global_lease: bool,
}

impl SessionLease {
    fn acquire() -> Result<Self> {
        LINUX_SESSION_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| backend_error(LinuxError::SessionActive))?;
        Ok(Self {
            owns_global_lease: true,
        })
    }

    /// A lease for sessions that never trace, such as post-mortem core dumps.
    const fn detached() -> Self {
        Self {
            owns_global_lease: false,
        }
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        if !self.owns_global_lease {
            return;
        }
        assert!(
            LINUX_SESSION_ACTIVE.swap(false, Ordering::AcqRel),
            "Linux tracing session lease was active"
        );
    }
}

struct BreakpointSite {
    original_byte: u8,
    installed: bool,
    owners: BTreeSet<BreakpointOwner>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum BreakpointOwner {
    User(BreakpointId),
    Plan(ExecutionId),
    /// The dynamic loader's report of each change to the loaded libraries.
    Loader,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeThreadState {
    Starting,
    Running,
    StopRequested,
    Stopped,
    Exiting,
}

#[derive(Debug, Clone)]
enum ExpectedStop {
    InitialExec,
    /// A process launched through its exec, which has not happened yet.
    AdoptedExec,
    InitialAttach,
    None,
    BreakpointRepair {
        address: VirtualAddress,
    },
    AwaitBreakpoint {
        address: VirtualAddress,
    },
    UserStep {
        kind: StepKind,
    },
}

struct TraceThread {
    state: NativeThreadState,
    expected: ExpectedStop,
    pending_signal: Option<PendingSignal>,
    reason: Option<StopReason>,
    stopped_at_breakpoint: Option<VirtualAddress>,
    awaiting_breakpoint: Option<VirtualAddress>,
    debugger_stop_pending: bool,
    /// The watch-plan generation programmed into this thread's debug
    /// registers. `None` means the debugger never programmed them.
    armed: Option<u64>,
    /// Watchpoints whose slots this thread hit since its last public stop.
    watch_hits: BTreeSet<WatchpointId>,
    /// The thread's name as of its start or the last published stop.
    name: Option<Arc<str>>,
}

impl TraceThread {
    const fn starting(expected: ExpectedStop) -> Self {
        Self {
            state: NativeThreadState::Starting,
            expected,
            pending_signal: None,
            reason: None,
            stopped_at_breakpoint: None,
            awaiting_breakpoint: None,
            debugger_stop_pending: false,
            armed: None,
            watch_hits: BTreeSet::new(),
            name: None,
        }
    }
}

/// The process-wide watchpoints and the debug-register plan every thread
/// carries.
#[derive(Default)]
struct WatchState {
    plan: DebugRegisterPlan,
    generation: u64,
    watchpoints: BTreeMap<WatchpointId, WatchRecord>,
}

struct WatchRecord {
    watchpoint: Watchpoint,
    frame: Option<FrameScopeEvidence>,
    /// The watched bytes when the debugger last observed them.
    observed: Option<Arc<[u8]>>,
}

#[derive(Clone, Copy)]
struct PendingSignal {
    signal: Signal,
    code: i32,
    sender: Option<i32>,
    fault_address: Option<u64>,
}

#[derive(Clone, Copy)]
struct SignalMetadata {
    code: i32,
    sender: Option<i32>,
    /// The faulting address of a synchronous fault.
    fault_address: Option<u64>,
}

// Addresses print in hex, as `VirtualAddress` does.
impl fmt::Debug for PendingSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingSignal")
            .field("signal", &self.signal)
            .field("code", &self.code)
            .field("sender", &self.sender)
            .field("fault_address", &HexAddress(self.fault_address))
            .finish()
    }
}

impl fmt::Debug for SignalMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignalMetadata")
            .field("code", &self.code)
            .field("sender", &self.sender)
            .field("fault_address", &HexAddress(self.fault_address))
            .finish()
    }
}

struct HexAddress(Option<u64>);

impl fmt::Debug for HexAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(address) => write!(f, "Some({address:#x})"),
            None => f.write_str("None"),
        }
    }
}

#[derive(Debug)]
struct RawStopRecord {
    status: String,
    siginfo: std::result::Result<SignalMetadata, Errno>,
}

#[derive(Debug)]
enum ClassifiedStop {
    ThreadStart,
    SignalDelivery(PendingSignal),
    GroupStop(Signal),
    Breakpoint(VirtualAddress),
    Watch(BTreeSet<WatchpointId>),
    Trace {
        /// Watchpoints hit by the stepped instruction.
        watch: BTreeSet<WatchpointId>,
    },
    DebuggerRequested,
    /// SIGKILL took the thread out of the reported stop; its exit follows.
    Superseded,
    Unclassifiable(RawStopRecord),
}

#[derive(Debug, Clone)]
struct StepStart {
    source: Option<SourceLocation>,
    code_instance: Option<CodeInstanceId>,
    physical_instance: Option<CodeInstanceId>,
    activation: Option<VirtualAddress>,
    plan_addresses: BTreeSet<VirtualAddress>,
    epilogue_traversal: Option<EpilogueTraversal>,
    return_traversal: Option<ReturnTraversal>,
    /// Where a signal handler returns to the instruction it interrupted.
    signal_guard: Option<SignalGuard>,
    /// For a step over a call instruction, the return address and the stack
    /// pointer the call returns with.
    call_return: Option<(VirtualAddress, u64)>,
}

/// Whether a step kind executes machine instructions rather than source
/// lines.
const fn steps_instructions(kind: StepKind) -> bool {
    matches!(kind, StepKind::Instruction | StepKind::OverInstruction)
}

/// The instruction a delivered signal interrupted during a step, and the
/// stack pointer its handler restores on returning there.
#[derive(Debug, Clone, Copy)]
struct SignalGuard {
    address: VirtualAddress,
    stack: u64,
}

#[derive(Debug, Clone)]
struct ReturnTraversal {
    /// A frame's return address, proven by agreeing CFI and ABI stack-slot
    /// sources.
    return_address: VirtualAddress,
    /// The activation whose return reaches `return_address`. For a tail call
    /// this is the step's starting activation; for a regular call it is the
    /// nested callee activation.
    guarded_activation: VirtualAddress,
    retire_return_after_repair: bool,
}

#[derive(Debug, Clone)]
struct EpilogueTraversal {
    /// The caller instruction is always a guard destination. It is only a
    /// source-step destination when it is also present in `completion_addresses`.
    return_address: VirtualAddress,
    completion_addresses: BTreeSet<VirtualAddress>,
    retire_return_after_repair: bool,
}

/// How a stopped thread resumes.
#[derive(Debug, Clone, Copy)]
enum Resume {
    Continue,
    Step,
}

#[derive(Debug, Clone)]
enum ActiveKind {
    Launch,
    Continue,
    Step {
        thread: Pid,
        kind: StepKind,
        start: Box<StepStart>,
        /// The stepping thread executed an instruction whose effect on the
        /// step has not been evaluated yet, because a breakpoint repair or
        /// an internal stop intervened.
        progress_owed: bool,
    },
}

struct ActiveExecution {
    id: ExecutionId,
    kind: ActiveKind,
    scope: ResumeScope,
    resume_threads: BTreeSet<Pid>,
}

struct RepairGroup {
    address: VirtualAddress,
    remaining: VecDeque<Pid>,
    current: Option<Pid>,
    site_removed: bool,
}

/// Stops every thread of the inferior, then either publishes a stop or,
/// when no thread produced a visible reason, applies its edits and resumes
/// the active execution as if nothing happened.
struct StopBarrier {
    triggering_thread: Pid,
    /// The stop to publish, or `None` while the stop is internal.
    reason: Option<StopReason>,
    /// Whether a client paused, so the stop is published as a pause when
    /// every reason it would have published is dropped.
    paused: bool,
    /// Why the active execution ended during the stop, such as its
    /// stepping thread exiting. The execution cannot resume, so the stop
    /// publishes this when every other reason is dropped.
    ended: Option<StopReason>,
    /// Client edits applied once every thread is stopped.
    edits: Vec<Edit>,
}

impl StopBarrier {
    /// A barrier that publishes `reason` from `triggering_thread`.
    fn visible(triggering_thread: Pid, reason: StopReason) -> Self {
        Self {
            triggering_thread,
            paused: reason == StopReason::Pause,
            reason: Some(reason),
            ended: None,
            edits: Vec::new(),
        }
    }
}

/// A breakpoint or watchpoint change that needs every thread stopped.
enum Edit {
    AddBreakpoint {
        spec: crate::BreakpointSpec,
        options: Box<crate::BreakpointOptions>,
        reply: Reply<Breakpoint>,
    },
    RemoveBreakpoint {
        id: BreakpointId,
        reply: Reply<Breakpoint>,
    },
    RemoveAllBreakpoints {
        reply: Reply<Arc<[Breakpoint]>>,
    },
    AddWatchpoint {
        spec: crate::WatchpointSpec,
        access: WatchAccess,
        reply: Reply<Watchpoint>,
    },
    RemoveWatchpoint {
        id: WatchpointId,
        reply: Reply<Watchpoint>,
    },
    RemoveAllWatchpoints {
        reply: Reply<Arc<[Watchpoint]>>,
    },
    /// Bring modules and breakpoints up to date after the loader changed
    /// the loaded libraries.
    RefreshModules,
}

/// The data object an expression's longest matching name prefix selected.
struct ExpressionRoot {
    /// How many leading named steps form the root's name.
    components: usize,
    kind: ExpressionRootKind,
}

enum ExpressionRootKind {
    /// A local or parameter of the inspected logical frame.
    Local {
        name: String,
        /// The module describing the frame's function.
        module: crate::ModuleId,
        /// The frame's address in the module's image.
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
    },
    Global(GlobalVariableReference),
}

struct PublicStop {
    id: StopId,
    triggering_thread: Pid,
    reason: StopReason,
    presentations: BTreeMap<Pid, FramePresentation>,
    /// Frames selected by clients; an absent thread has its innermost
    /// frame selected.
    selected_frames: BTreeMap<Pid, StackFrameId>,
}

/// Allocates stop identifiers that are unique for the whole process lifetime.
///
/// A `DereferenceReference` (or any stopped-state capability) is a public value
/// that can outlive the `Controller` that minted it. A per-controller counter
/// would restart at the same value in a sequentially-created controller, so a
/// stale capability whose thread and module identities happened to recur could
/// authenticate against a newer inferior. Allocating process-wide guarantees no
/// two stops ever share an id, closing that ABA reuse gap at its source.
static NEXT_STOP_ID: AtomicU64 = AtomicU64::new(1);

fn allocate_stop_id() -> StopId {
    // `fetch_update` leaves the counter unchanged when the closure returns
    // `None`, so exhaustion cannot wrap the atomic to zero and reissue low ids.
    #[allow(
        deprecated,
        reason = "try_update is not yet stable on the pinned toolchain"
    )]
    let previous = NEXT_STOP_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .expect("stop identifiers exhausted");
    StopId::new(
        previous
            .checked_add(1)
            .expect("successful atomic update proved the increment fits"),
    )
}

struct Inferior {
    origin: InferiorOrigin,
    tgid: Pid,
    loaded_module: LoadedModule,
    breakpoints: BTreeMap<VirtualAddress, BreakpointSite>,
    /// Every site each execution's plan installed, so that ending the plan
    /// visits only those. A site the plan already released may remain.
    plan_sites: BTreeMap<ExecutionId, BTreeSet<VirtualAddress>>,
    threads: BTreeMap<Pid, TraceThread>,
    /// Threads that left the inferior and whose exit status is still due.
    retired_threads: BTreeSet<Pid>,
    /// Initial stops of new threads or fork children that arrived before
    /// the event that announces them.
    unowned_stops: BTreeMap<Pid, WaitEvent>,
    /// Threads that began exiting before the event that announces them.
    vanished_threads: BTreeSet<Pid>,
    /// Listed threads that exited before an attach could seize them, such
    /// as a leader that exited before the rest of its process. No status
    /// of theirs is due.
    unseized_threads: BTreeSet<Pid>,
    /// Fork children announced before their initial stop arrived, with the
    /// breakpoint sites each inherited.
    fork_children: BTreeMap<Pid, Vec<(VirtualAddress, u8)>>,
    waiter: Option<Waiter>,
    active: Option<ActiveExecution>,
    repairs: VecDeque<RepairGroup>,
    barrier: Option<StopBarrier>,
    public_stop: Option<PublicStop>,
    selected_thread: Option<Pid>,
    next_execution: u64,
    exec_unsupported: bool,
    /// The loader's breakpoint, once the loader is known.
    loader_site: Option<VirtualAddress>,
    watch: WatchState,
    /// The signal the debugger sent to end the inferior, which never stops
    /// it whatever its policy.
    terminating: Option<Terminating>,
}

/// The debugger asked the inferior to end with `signal`. Programs often
/// handle it by cleaning up and raising it again with its default action,
/// as Go's runtime does, so every delivery of it passes silently until the
/// user sees a stop after the first one.
#[derive(Clone, Copy, Debug)]
struct Terminating {
    signal: Signal,
    delivered: bool,
}

impl Inferior {
    /// Creates an inferior with no execution, stop, breakpoints, or watchpoints.
    fn new(
        origin: InferiorOrigin,
        tgid: Pid,
        loaded_module: LoadedModule,
        threads: BTreeMap<Pid, TraceThread>,
        waiter: Option<Waiter>,
    ) -> Self {
        Self {
            origin,
            tgid,
            loaded_module,
            breakpoints: BTreeMap::new(),
            plan_sites: BTreeMap::new(),
            threads,
            retired_threads: BTreeSet::new(),
            unowned_stops: BTreeMap::new(),
            vanished_threads: BTreeSet::new(),
            unseized_threads: BTreeSet::new(),
            fork_children: BTreeMap::new(),
            waiter,
            active: None,
            repairs: VecDeque::new(),
            barrier: None,
            public_stop: None,
            selected_thread: None,
            next_execution: 0,
            exec_unsupported: false,
            loader_site: None,
            watch: WatchState::default(),
            terminating: None,
        }
    }

    /// A stopped thread through which to read and write the shared address
    /// space: the leader when it is stopped, otherwise any stopped thread.
    /// While some threads run, only a stopped thread accepts ptrace requests.
    fn memory_thread(&self) -> Pid {
        let stopped = |thread: &TraceThread| matches!(thread.state, NativeThreadState::Stopped);
        if self.threads.get(&self.tgid).is_some_and(stopped) {
            return self.tgid;
        }
        self.threads
            .iter()
            .find_map(|(&pid, thread)| stopped(thread).then_some(pid))
            .unwrap_or(self.tgid)
    }

    fn thread(&self, pid: Pid) -> Result<&TraceThread> {
        self.threads
            .get(&pid)
            .ok_or_else(|| Error::UnknownThread(debug_thread_id(pid)))
    }

    fn thread_mut(&mut self, pid: Pid) -> Result<&mut TraceThread> {
        self.threads
            .get_mut(&pid)
            .ok_or_else(|| Error::UnknownThread(debug_thread_id(pid)))
    }

    /// Removes every reference to a thread that exited while others live on.
    /// Returns whether the thread's breakpoint repair step was interrupted,
    /// in which case execution must advance to the next repair.
    fn forget_thread(&mut self, pid: Pid, status: &ExitStatus) -> bool {
        if let Some(active) = self.active.as_mut() {
            active.resume_threads.remove(&pid);
        }
        let mut interrupted = false;
        for group in &mut self.repairs {
            group.remaining.retain(|&waiting| waiting != pid);
            if group.current == Some(pid) {
                group.current = None;
                interrupted = true;
            }
        }
        if self.selected_thread == Some(pid) {
            self.selected_thread = None;
        }
        if let Some(stop) = self.public_stop.as_mut() {
            stop.presentations.remove(&pid);
        }
        // A barrier is presented from its triggering thread, which must live.
        // An internal stop still has nothing to present.
        let replacement = self
            .threads
            .iter()
            .find(|(_, thread)| matches!(thread.state, NativeThreadState::Stopped))
            .or_else(|| self.threads.iter().next())
            .map(|(&pid, _)| pid);
        if let Some(barrier) = self
            .barrier
            .as_mut()
            .filter(|barrier| barrier.triggering_thread == pid)
            && let Some(replacement) = replacement
        {
            barrier.triggering_thread = replacement;
            // An internal stop stays internal.
            if barrier.reason.is_some() {
                barrier.reason = Some(StopReason::ThreadExited {
                    thread_id: debug_thread_id(pid),
                    status: status.clone(),
                });
            }
        }
        interrupted
    }

    /// The edits an internal stop holds until every thread stopped.
    fn take_pending_edits(&mut self) -> Vec<Edit> {
        self.barrier
            .as_mut()
            .map(|barrier| std::mem::take(&mut barrier.edits))
            .unwrap_or_default()
    }

    /// The breakpoint sites a process forked now inherits, with their
    /// original bytes. A site lifted for a repair is included, since
    /// restoring a byte that is already in place changes nothing.
    fn inherited_sites(&self) -> Vec<(VirtualAddress, u8)> {
        self.breakpoints
            .iter()
            .map(|(&address, site)| (address, site.original_byte))
            .collect()
    }

    /// Ends `pid`'s step over the front repair group's breakpoint at `address`.
    fn finish_current_repair(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        self.thread_mut(pid)?.stopped_at_breakpoint = None;
        let group = self
            .repairs
            .front_mut()
            .filter(|group| group.address == address && group.current == Some(pid))
            .ok_or_else(|| {
                backend_error(LinuxError::UnexpectedWait(format!(
                    "breakpoint repair by {pid} at {address} does not match the active repair"
                )))
            })?;
        group.current = None;
        Ok(())
    }
}

/// Collects an inferior's wait statuses for the controller: a thread in a
/// live session, or nothing when whoever drives the controller reports
/// statuses itself, as test fakes and the simulator do.
struct Waiter {
    thread: Option<WaiterThread>,
}

struct WaiterThread {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl Waiter {
    /// A waiter whose statuses the controller's driver delivers.
    #[cfg(test)]
    const fn external() -> Self {
        Self { thread: None }
    }

    fn stop_and_join(self) -> Result<()> {
        let Some(thread) = self.thread else {
            return Ok(());
        };
        thread.stop.store(true, Ordering::Release);
        thread.handle.thread().unpark();
        thread
            .handle
            .join()
            .map_err(|_| Error::BackendThreadPanicked)
    }

    fn join(self) -> Result<()> {
        self.thread.map_or(Ok(()), |thread| {
            thread
                .handle
                .join()
                .map_err(|_| Error::BackendThreadPanicked)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InferiorOrigin {
    Launched,
    Attached,
    /// Seized while waiting to exec the executable, and owned as a launch.
    LaunchedByExec,
    PostMortem,
}

impl InferiorOrigin {
    /// Whether the process dies with the debugger.
    const fn owned(self) -> bool {
        matches!(self, Self::Launched | Self::LaunchedByExec)
    }

    /// Whether its threads were seized, so they stop by `PTRACE_INTERRUPT`
    /// and report group-stops as events.
    const fn seized(self) -> bool {
        matches!(self, Self::Attached | Self::LaunchedByExec)
    }
}

struct RuntimeModule {
    loaded: LoadedModule,
    image: Arc<ModuleImage>,
    unwind: Arc<dyn UnwindInfo>,
    variables: Arc<dyn VariableInfo>,
    link_map: Option<VirtualAddress>,
}

#[derive(Debug, thiserror::Error)]
enum LinuxError {
    #[error("system tracing operation failed: {0}")]
    System(#[from] nix::Error),
    #[error("failed to parse executable object: {0}")]
    Object(#[from] object::Error),
    #[error("unexpected wait status: {0}")]
    UnexpectedWait(String),
    #[error("could not determine load bias for {0}")]
    LoadBias(PathBuf),
    #[error("invalid process mapping: {0}")]
    InvalidMapping(String),
    #[error("{0} is malformed")]
    ProcFile(String),
    #[error("dynamic-loader rendezvous is malformed: {0}")]
    LoaderRendezvous(String),
    #[error("another Linux tracing session is already active in this process")]
    SessionActive,
    #[error("the target's thread list did not stabilize while attaching")]
    AttachThreadsUnstable,
    #[error("unsupported clone created a different thread group {0}")]
    UnsupportedClone(i32),
    #[error("floating-point register reads are unsupported by this tracing effect")]
    UnsupportedFloatingRegisters,
    #[error("only general registers can be changed")]
    UnsupportedRegisterWrite,
    #[error("the inferior replaced its executable image; loading the new image is not supported")]
    UnsupportedExec,
    #[error("could not determine the caller frame: {0}")]
    CallerUnavailable(UnwindTermination),
    #[error("breakpoint site {0} was not found")]
    BreakpointSiteMissing(VirtualAddress),
    #[error("breakpoint site {0} did not have the expected owner")]
    BreakpointOwnerMissing(VirtualAddress),
    #[error("logical breakpoint identifiers were exhausted")]
    BreakpointIdExhausted,
    #[error("watchpoint identifiers were exhausted")]
    WatchpointIdExhausted,
    #[error("watchpoint arming failed ({cause}) and rollback also failed ({recovery})")]
    WatchpointArmRecovery { cause: String, recovery: String },
    #[error("loaded module identifiers were exhausted")]
    ModuleIdExhausted,
    #[error("module image identifiers were exhausted")]
    ModuleImageIdExhausted,
    #[error("breakpoint installation failed ({cause}) and rollback also failed ({recovery})")]
    BreakpointInstallRecovery { cause: String, recovery: String },
    #[error("breakpoint removal failed ({cause}) and rollback also failed ({recovery})")]
    BreakpointRemoveRecovery { cause: String, recovery: String },
    #[error("resume failed ({cause}) and recovery also failed ({recovery})")]
    ResumeRecovery { cause: String, recovery: String },
    #[error("logical memory read of {size} bytes exceeds the {maximum}-byte limit")]
    MemoryReadTooLarge { size: usize, maximum: usize },
    #[error("target memory is inaccessible at {address}")]
    MemoryInaccessible { address: VirtualAddress },
    #[error("the core dump does not describe thread {0}")]
    UnknownCoreThread(i32),
    #[error("the core dump saved no floating-point registers for thread {0}")]
    CoreFloatingRegistersUnsaved(i32),
    #[error("the process exited before its first stop: {0:?}")]
    ExitedBeforeStop(ExitStatus),
    #[error("the process executed a program other than {}", .0.display())]
    ExecutedAnotherProgram(PathBuf),
}

struct Controller<P: InspectionOps> {
    _lease: SessionLease,
    executable: Arc<PathBuf>,
    executable_data: Arc<[u8]>,
    executable_identity: FileIdentity,
    expected_process_start_time: Option<u64>,
    module_image: Arc<ModuleImage>,
    unwind_info: Arc<dyn UnwindInfo>,
    modules: BTreeMap<crate::ModuleId, RuntimeModule>,
    /// The canonical path and load bias computed for each mapping, so known
    /// modules are not re-read from disk at every stop.
    mapped_modules: BTreeMap<ModuleMapping, (PathBuf, u64)>,
    next_module_id: u32,
    next_image_id: u32,
    messages: mpsc::Receiver<ControllerMessage>,
    message_sender: mpsc::Sender<ControllerMessage>,
    events: EventSender,
    ptrace: P,
    inferior: Option<Inferior>,
    breakpoints: breakpoints::UserBreakpoints,
    next_breakpoint_id: u64,
    next_watchpoint_id: u64,
    launch_reply: Option<Reply<ExecutionId>>,
    attach_reply: Option<Reply<StopId>>,
    /// How many times the attach in progress found threads it had not
    /// traced.
    attach_rescans: u32,
    /// Whether the session is ending: wait events then only advance the kill
    /// or detach, and the controller exits once the inferior is gone.
    shutting_down: bool,
    shutdown_reply: Option<Reply<()>>,
    /// The client waiting for a killed inferior to be gone.
    kill_reply: Option<Reply<()>>,
    signals: SignalPolicies,
    revision: u64,
}

pub fn spawn_controller(
    executable: ExecutableSource,
    debug_info: DebugInfo,
    channels: ControllerChannels,
) -> Result<JoinHandle<()>> {
    let lease = SessionLease::acquire()?;

    Ok(thread::Builder::new()
        .name(CONTROLLER_THREAD_NAME.into())
        .spawn(move || {
            record!("started for {}", executable.display_path.display());
            #[cfg(debug_assertions)]
            let ptrace = recorded::Recorded(LinuxPtrace::new());
            #[cfg(not(debug_assertions))]
            let ptrace = LinuxPtrace::new();
            Controller::new(lease, executable, debug_info, channels, ptrace).run();
            record!("exited");
        })?)
}

impl<P: InspectionOps> Controller<P> {
    fn new(
        lease: SessionLease,
        executable: ExecutableSource,
        debug_info: DebugInfo,
        channels: ControllerChannels,
        ptrace: P,
    ) -> Self {
        let DebugInfo {
            image: module_image,
            unwind: unwind_info,
            variables: variable_info,
        } = debug_info;
        let main = RuntimeModule {
            loaded: LoadedModule::main(module_image.id(), 0),
            image: Arc::clone(&module_image),
            unwind: Arc::clone(&unwind_info),
            variables: variable_info,
            link_map: None,
        };
        Self {
            _lease: lease,
            executable: executable.display_path,
            executable_data: executable.data,
            executable_identity: executable.identity,
            expected_process_start_time: executable.process_start_time,
            module_image,
            unwind_info,
            modules: BTreeMap::from([(main.loaded.id, main)]),
            mapped_modules: BTreeMap::new(),
            next_module_id: 1,
            next_image_id: 1,
            messages: channels.receiver,
            message_sender: channels.sender,
            events: channels.events,
            ptrace,
            inferior: None,
            breakpoints: breakpoints::UserBreakpoints::default(),
            next_breakpoint_id: 1,
            next_watchpoint_id: 1,
            launch_reply: None,
            attach_reply: None,
            attach_rescans: 0,
            shutting_down: false,
            shutdown_reply: None,
            kill_reply: None,
            signals: SignalPolicies::default(),
            revision: 0,
        }
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    fn run(mut self) {
        while let Some(message) = self.messages.blocking_recv() {
            if !self.handle_message(message) {
                return;
            }
        }
        // The controller holds a request sender for its waiter, so the queue
        // never closes; dropping the `Debugger` sends a shutdown request.
    }

    /// Serves one queued request or wait status, and returns whether the
    /// controller keeps running. A driver that owns the queue, such as the
    /// simulator, delivers messages one at a time through this.
    fn handle_message(&mut self, message: ControllerMessage) -> bool {
        match message {
            ControllerMessage::Request(request) => {
                record!("request {}", request.describe());
                self.handle_request(request)
            }
            ControllerMessage::Wait(status) => self.handle_wait(status),
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the exhaustive request dispatcher keeps protocol routing in one place"
    )]
    fn handle_request(&mut self, request: Request) -> bool {
        match request {
            Request::AddBreakpoint {
                spec,
                options,
                reply,
            } => {
                self.edit(Edit::AddBreakpoint {
                    spec,
                    options,
                    reply,
                });
            }
            Request::SetBreakpointCondition {
                id,
                condition,
                reply,
            } => {
                let _ = reply.send(self.set_breakpoint_condition(id, condition));
            }
            Request::SetBreakpointHitCondition {
                id,
                hit_condition,
                reply,
            } => {
                let _ = reply.send(self.set_breakpoint_hit_condition(id, hit_condition));
            }
            Request::RemoveBreakpoint { id, reply } => {
                self.edit(Edit::RemoveBreakpoint { id, reply });
            }
            Request::RemoveAllBreakpoints { reply } => {
                self.edit(Edit::RemoveAllBreakpoints { reply });
            }
            Request::AddWatchpoint {
                spec,
                access,
                reply,
            } => {
                self.edit(Edit::AddWatchpoint {
                    spec,
                    access,
                    reply,
                });
            }
            Request::RemoveWatchpoint { id, reply } => {
                self.edit(Edit::RemoveWatchpoint { id, reply });
            }
            Request::RemoveAllWatchpoints { reply } => {
                self.edit(Edit::RemoveAllWatchpoints { reply });
            }
            Request::Launch { options, reply } => self.launch(*options, reply),
            Request::Attach { process_id, reply } => self.attach(process_id, reply),
            Request::LaunchByExec {
                process_id,
                stop_at_entry,
                release,
                reply,
            } => self.launch_by_exec(process_id, stop_at_entry, release, reply),
            Request::Continue {
                process_id,
                stop_id,
                scope,
                exception,
                reply,
            } => self.resume(process_id, stop_id, scope, exception, reply),
            Request::Step {
                process_id,
                stop_id,
                thread_id,
                frame,
                kind,
                scope,
                exception,
                reply,
            } => match debug_pid(thread_id) {
                Ok(pid) => self.step(
                    process_id, stop_id, pid, frame, kind, scope, exception, reply,
                ),
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            },
            Request::Pause { process_id, reply } => self.pause(process_id, reply),
            Request::WriteWord {
                process_id,
                stop_id,
                address,
                value,
                reply,
            } => {
                let result = self.write_word(process_id, stop_id, address, value);
                let _ = reply.send(result);
            }
            Request::WriteMemory {
                process_id,
                stop_id,
                address,
                bytes,
                reply,
            } => {
                let _ = reply.send(self.write_memory(process_id, stop_id, address, &bytes));
            }
            Request::Assign {
                stop_id,
                thread_id,
                frame,
                expression,
                value,
                reply,
            } => {
                let result = debug_pid(thread_id)
                    .and_then(|pid| self.assign(stop_id, pid, frame, &expression, &value));
                let _ = reply.send(result);
            }
            Request::Shutdown { reply } => {
                self.begin_shutdown(Some(reply));
                return self.inferior.is_some();
            }
            Request::Kill { reply } => self.kill(reply),
            Request::Terminate { reply } => {
                let _ = reply.send(self.terminate());
            }
            request => self.handle_inspection_request(request),
        }

        true
    }
}

impl<P: InspectionOps> Controller<P> {
    /// Answers a read-only request against the current stopped snapshot.
    /// Live and post-mortem sessions route every other request themselves.
    #[expect(
        clippy::too_many_lines,
        reason = "the exhaustive inspection dispatcher keeps protocol routing in one place"
    )]
    fn handle_inspection_request(&mut self, request: Request) {
        match request {
            Request::ReadMemory {
                process_id,
                stop_id,
                address,
                byte_count,
                reply,
            } => {
                let _ = reply.send(self.read_memory(process_id, stop_id, address, byte_count));
            }
            Request::ReadWord {
                process_id,
                stop_id,
                address,
                reply,
            } => {
                let _ = reply.send(self.read_word(process_id, stop_id, address));
            }
            Request::LoadedModule { reply } => {
                let _ = reply.send(self.loaded_module());
            }
            Request::LoadedModules { reply } => {
                let _ = reply.send(self.loaded_modules());
            }
            Request::ModuleImage { module, reply } => {
                let _ = reply.send(
                    self.modules
                        .get(&module)
                        .map(|module| Arc::clone(&module.image))
                        .ok_or(Error::ModuleNotLoaded(module)),
                );
            }
            Request::Disassemble {
                query,
                stop_id,
                thread_id,
                reply,
            } => {
                let _ = reply.send(
                    debug_pid(thread_id).and_then(|pid| self.disassemble(stop_id, pid, query)),
                );
            }
            Request::DescribeAddress {
                stop_id,
                address,
                reply,
            } => {
                let _ = reply.send(self.describe_address(stop_id, address));
            }
            Request::StoppedLocation {
                stop_id,
                thread_id,
                frame,
                reply,
            } => {
                let _ = reply.send(
                    debug_pid(thread_id).and_then(|pid| self.stopped_location(stop_id, pid, frame)),
                );
            }
            Request::Snapshot { reply } => {
                let _ = reply.send(Ok(self.snapshot()));
            }
            Request::StoppedSelection { reply } => {
                let _ = reply.send(self.stopped_selection());
            }
            Request::Backtrace {
                stop_id,
                thread_id,
                reply,
            } => {
                let _ =
                    reply.send(debug_pid(thread_id).and_then(|pid| self.backtrace(stop_id, pid)));
            }
            Request::Registers {
                stop_id,
                thread_id,
                frame,
                reply,
            } => {
                let _ = reply
                    .send(debug_pid(thread_id).and_then(|pid| self.registers(stop_id, pid, frame)));
            }
            Request::Variables {
                query,
                limits,
                stop_id,
                thread_id,
                frame,
                reply,
            } => {
                let _ = reply.send(
                    debug_pid(thread_id)
                        .and_then(|pid| self.variables(stop_id, pid, frame, &query, limits)),
                );
            }
            Request::Inspect {
                expression,
                limits,
                stop_id,
                thread_id,
                frame,
                reply,
            } => {
                let _ =
                    reply
                        .send(debug_pid(thread_id).and_then(|pid| {
                            self.inspect(stop_id, pid, frame, &expression, limits)
                        }));
            }
            Request::InspectRange {
                expression,
                range,
                limits,
                stop_id,
                thread_id,
                frame,
                reply,
            } => {
                let _ = reply.send(debug_pid(thread_id).and_then(|pid| {
                    self.inspect_range(stop_id, pid, frame, &expression, range, limits)
                }));
            }
            Request::Dereference {
                reference,
                limits,
                reply,
            } => {
                let _ = reply.send(self.dereference(&reference, limits));
            }
            Request::ValueChildren {
                reference,
                query,
                limits,
                reply,
            } => {
                let _ = reply.send(self.value_children(&reference, &query, limits));
            }
            Request::Globals { query, reply } => {
                let _ = reply.send(self.globals(&query));
            }
            Request::SelectThread {
                stop_id,
                thread_id,
                reply,
            } => {
                let result = debug_pid(thread_id).and_then(|pid| self.select_thread(stop_id, pid));
                let _ = reply.send(result);
            }
            Request::SelectFrame {
                stop_id,
                thread_id,
                frame,
                reply,
            } => {
                let result =
                    debug_pid(thread_id).and_then(|pid| self.select_frame(stop_id, pid, frame));
                let _ = reply.send(result);
            }
            Request::ResolveWatchTarget {
                expression,
                stop_id,
                thread_id,
                frame,
                reply,
            } => {
                let _ =
                    reply.send(debug_pid(thread_id).and_then(|pid| {
                        self.resolve_watch_target(stop_id, pid, frame, &expression)
                    }));
            }
            Request::SignalPolicy { signal, reply } => {
                let _ = reply.send(
                    Signal::from_code(signal)
                        .map(|signal| self.signals.get(signal))
                        .ok_or(Error::UnknownSignal(signal)),
                );
            }
            Request::SetSignalPolicy {
                signal,
                policy,
                reply,
            } => {
                let _ = reply.send(
                    Signal::from_code(signal)
                        .map(|signal| self.signals.set(signal, policy))
                        .ok_or(Error::UnknownSignal(signal)),
                );
            }
            Request::AddBreakpoint { .. }
            | Request::SetBreakpointHitCondition { .. }
            | Request::SetBreakpointCondition { .. }
            | Request::RemoveBreakpoint { .. }
            | Request::RemoveAllBreakpoints { .. }
            | Request::AddWatchpoint { .. }
            | Request::RemoveWatchpoint { .. }
            | Request::RemoveAllWatchpoints { .. }
            | Request::Launch { .. }
            | Request::Attach { .. }
            | Request::LaunchByExec { .. }
            | Request::Continue { .. }
            | Request::Step { .. }
            | Request::Pause { .. }
            | Request::WriteWord { .. }
            | Request::WriteMemory { .. }
            | Request::Assign { .. }
            | Request::Kill { .. }
            | Request::Terminate { .. }
            | Request::Shutdown { .. } => {
                unreachable!("run-control requests are routed by the session dispatcher")
            }
        }
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    fn handle_wait(&mut self, status: WaitEvent) -> bool {
        if self.shutting_down {
            return self.handle_shutdown_wait(status);
        }

        // The status reported a stop, whatever its thread is counted as.
        let reported = (!matches!(
            status,
            WaitEvent::Exited(..) | WaitEvent::Signaled(..) | WaitEvent::Continued(_)
        ))
        .then(|| status.pid());
        if let Err(error) = self.process_wait(status)
            && !(is_vanished_tracee(&error)
                && (self.release_superseded_threads(reported) || self.every_thread_exiting()))
        {
            self.fail_inferior(error);
        }

        true
    }

    /// Whether `error` came from SIGKILL taking threads out of the stop the
    /// controller counts them in, so the address space is dying and needs
    /// nothing restored. The failure's handler releases them.
    fn lost_to_sigkill(&self, error: &Error) -> bool {
        is_vanished_tracee(error)
            && self.inferior.as_ref().is_some_and(|inferior| {
                inferior.threads.iter().any(|(&pid, thread)| {
                    matches!(thread.state, NativeThreadState::Stopped)
                        && is_superseded(&self.ptrace.signal_metadata(pid))
                })
            })
    }

    /// Whether every live thread passed its exit event, leaving none to
    /// read or write the dying address space through.
    fn every_thread_exiting(&self) -> bool {
        self.inferior.as_ref().is_some_and(|inferior| {
            inferior
                .threads
                .values()
                .all(|thread| matches!(thread.state, NativeThreadState::Exiting))
        })
    }

    /// Finds the threads counted as stopped, or whose reported stop is
    /// being handled, that SIGKILL took out of their stop, typically a
    /// sibling's `exit_group` while a stop was handled, which explains a
    /// ptrace request failing with ESRCH. Each runs on to its exit, which
    /// retires it. Returns whether any was found.
    fn release_superseded_threads(&mut self, handled: Option<Pid>) -> bool {
        let Some(inferior) = self.inferior.as_mut() else {
            return false;
        };
        let mut found = false;
        for (&pid, thread) in &mut inferior.threads {
            if (matches!(thread.state, NativeThreadState::Stopped) || handled == Some(pid))
                && is_superseded(&self.ptrace.signal_metadata(pid))
            {
                record!("{pid} left its stop while it was handled");
                thread.state = NativeThreadState::Running;
                found = true;
            }
        }
        found
    }

    fn process_wait(&mut self, status: WaitEvent) -> Result<()> {
        let pid = status.pid();
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let known = inferior.threads.contains_key(&pid);
        let retired = inferior.retired_threads.contains(&pid);

        if !known {
            // A retired thread's exit, a fork child, or a new thread's first
            // stop arriving before its parent's clone or fork event, which is
            // SIGSTOP for a launched process and PTRACE_EVENT_STOP for a
            // seized one.
            let stop = matches!(
                status,
                WaitEvent::Stopped(..) | WaitEvent::PtraceEvent(_, _, libc::PTRACE_EVENT_STOP)
            );
            let expected = stop
                || retired
                || inferior.fork_children.contains_key(&pid)
                || matches!(
                    status,
                    WaitEvent::PtraceEvent(_, _, libc::PTRACE_EVENT_EXIT)
                        | WaitEvent::Exited(..)
                        | WaitEvent::Signaled(..)
                );
            if expected && self.absorb_untracked_wait(&status) {
                return Ok(());
            }
            return Err(backend_error(LinuxError::UnexpectedWait(format!(
                "unowned {status:?}"
            ))));
        }
        if matches!(inferior.thread(pid)?.expected, ExpectedStop::AdoptedExec) {
            // Until its exec, a process launched through it runs as it
            // would untraced: signals are delivered and job control is
            // ignored.
            match status {
                WaitEvent::Stopped(pid, signal) => {
                    return self.ptrace.continue_execution(pid, Some(signal));
                }
                WaitEvent::PtraceEvent(pid, _, libc::PTRACE_EVENT_STOP) => {
                    return self.ptrace.continue_execution(pid, None);
                }
                _ => {}
            }
        }

        match status {
            WaitEvent::Exited(pid, code) => {
                self.handle_terminal(pid, ExitStatus::Code(i64::from(code)))
            }
            WaitEvent::Signaled(pid, signal, _) => {
                self.handle_terminal(pid, ExitStatus::Terminated(exception_info(signal)))
            }
            WaitEvent::PtraceEvent(pid, signal, libc::PTRACE_EVENT_STOP) => {
                self.handle_event_stop(pid, signal)
            }
            WaitEvent::PtraceEvent(pid, _, event) => self.handle_ptrace_event(pid, event),
            WaitEvent::PtraceSyscall(pid) => self.handle_classified_stop(
                pid,
                ClassifiedStop::Unclassifiable(RawStopRecord {
                    status: "ptrace syscall stop while syscall tracing is unsupported".to_owned(),
                    siginfo: Err(Errno::EINVAL),
                }),
            ),
            WaitEvent::Stopped(pid, signal) => {
                let initial = self
                    .inferior
                    .as_ref()
                    .and_then(|inferior| inferior.threads.get(&pid))
                    .is_some_and(|thread| matches!(thread.expected, ExpectedStop::InitialExec));
                if initial && signal == Signal::SIGTRAP {
                    self.handle_initial_stop(pid)
                } else {
                    let stop = self.classify_stop(pid, signal);
                    self.handle_classified_stop(pid, stop)
                }
            }
            other @ WaitEvent::Continued(_) => Err(backend_error(LinuxError::UnexpectedWait(
                format!("{other:?}"),
            ))),
        }
    }

    /// Routes a `PTRACE_EVENT_STOP`, which seized threads report for several
    /// unrelated reasons.
    fn handle_event_stop(&mut self, pid: Pid, signal: Signal) -> Result<()> {
        // SIGKILL, typically a sibling's `exit_group`, may have taken the
        // thread out of the stop since it was reported.
        if is_superseded(&self.ptrace.signal_metadata(pid)) {
            return self.handle_classified_stop(pid, ClassifiedStop::Superseded);
        }
        let thread = self
            .inferior
            .as_ref()
            .ok_or(Error::NotRunning)?
            .thread(pid)?;
        if matches!(thread.expected, ExpectedStop::InitialAttach) {
            return self.handle_initial_attach_stop(pid);
        }
        let stop = if matches!(thread.state, NativeThreadState::Starting)
            && matches!(thread.expected, ExpectedStop::None)
        {
            // Threads auto-attached to a seized process start here instead
            // of with SIGSTOP.
            ClassifiedStop::ThreadStart
        } else if thread.debugger_stop_pending {
            ClassifiedStop::DebuggerRequested
        } else if is_stopping_signal(signal) {
            // A seized thread reports a group-stop here, with its signal.
            ClassifiedStop::GroupStop(signal)
        } else if signal == Signal::SIGTRAP {
            // A `PTRACE_INTERRUPT` that reached a thread already in another
            // ptrace-stop, such as a clone event while attaching, is kept
            // until the thread resumes. It then stops before running any
            // instruction, after the debugger took the other stop for it.
            ClassifiedStop::DebuggerRequested
        } else {
            return self.handle_ptrace_event(pid, libc::PTRACE_EVENT_STOP);
        };
        self.handle_classified_stop(pid, stop)
    }
}

impl<P: InspectionOps> Controller<P> {
    fn snapshot(&self) -> StateSnapshot {
        let Some(inferior) = self.inferior.as_ref() else {
            return StateSnapshot {
                revision: self.revision,
                inferior: InferiorState::NotRunning,
                stop_id: None,
                selected_thread: None,
                selected_frame: None,
                threads: Arc::from([]),
                presentation: None,
                breakpoints: Arc::from(&*self.breakpoints),
                watchpoints: Arc::from([]),
            };
        };
        let process_id = process_id(inferior.tgid);
        let state = inferior.public_stop.as_ref().map_or_else(
            || InferiorState::Running {
                process_id,
                execution_id: inferior.active.as_ref().map(|active| active.id),
            },
            |stop| InferiorState::Stopped {
                process_id,
                stop_id: stop.id,
                thread_id: debug_thread_id(stop.triggering_thread),
                reason: stop.reason.clone(),
            },
        );
        let threads = inferior
            .threads
            .iter()
            .map(|(&pid, thread)| ThreadSnapshot {
                id: debug_thread_id(pid),
                name: thread.name.clone(),
                state: if matches!(thread.state, NativeThreadState::Stopped) {
                    ObservableThreadState::Stopped {
                        reason: thread.reason.clone(),
                    }
                } else {
                    ObservableThreadState::Running
                },
            })
            .collect::<Vec<_>>()
            .into();

        StateSnapshot {
            revision: self.revision,
            inferior: state,
            stop_id: inferior.public_stop.as_ref().map(|stop| stop.id),
            selected_thread: inferior.selected_thread.map(debug_thread_id),
            selected_frame: inferior
                .public_stop
                .as_ref()
                .map(|stop| selected_frame(inferior, stop)),
            threads,
            presentation: inferior.selected_thread.and_then(|pid| {
                inferior
                    .public_stop
                    .as_ref()
                    .and_then(|stop| stop.presentations.get(&pid))
                    .cloned()
            }),
            breakpoints: Arc::from(&*self.breakpoints),
            watchpoints: inferior
                .watch
                .watchpoints
                .values()
                .map(|record| record.watchpoint.clone())
                .collect::<Vec<_>>()
                .into(),
        }
    }

    /// The selection a snapshot reports, without copying the breakpoints
    /// and threads that every stopped request would otherwise pay for.
    fn stopped_selection(&self) -> Result<crate::StoppedSelection> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let stop = inferior.public_stop.as_ref().ok_or(Error::NotStopped)?;
        Ok(crate::StoppedSelection {
            process: process_id(inferior.tgid),
            stop: stop.id,
            thread: debug_thread_id(inferior.selected_thread.unwrap_or(stop.triggering_thread)),
            frame: selected_frame(inferior, stop),
        })
    }
}

/// The frame selected in the stop's selected thread: the innermost until a
/// client selects another.
fn selected_frame(inferior: &Inferior, stop: &PublicStop) -> StackFrameId {
    let thread = inferior.selected_thread.unwrap_or(stop.triggering_thread);
    stop.selected_frames
        .get(&thread)
        .copied()
        .unwrap_or(StackFrameId::INNERMOST)
}

impl<P: InspectionOps> Controller<P> {
    fn bump_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        let _ = self.events.send(DebuggerEvent::StateChanged {
            revision: self.revision,
        });
    }
}

fn validate_process(inferior: &Inferior, requested: ProcessId) -> Result<()> {
    if process_id(inferior.tgid) == requested {
        Ok(())
    } else {
        Err(Error::NotRunning)
    }
}

fn validate_public_stop(inferior: &Inferior, requested: Option<StopId>) -> Result<()> {
    match (&inferior.public_stop, requested) {
        (Some(current), Some(requested)) if current.id == requested => Ok(()),
        (Some(_), Some(_)) => Err(Error::StaleStop),
        (Some(_), None) => Ok(()),
        (None, _) => Err(Error::NotStopped),
    }
}

/// Rejects execution from a stop that cannot be resumed safely.
fn validate_resumable(inferior: &Inferior) -> Result<()> {
    if inferior
        .public_stop
        .as_ref()
        .is_some_and(|stop| matches!(stop.reason, StopReason::Unclassifiable { .. }))
    {
        return Err(Error::UnclassifiableStop);
    }
    validate_image_current(inferior)
}

/// Rejects requests that interpret the address space through the debugged
/// image after exec(2) replaced it; the new image is not loaded.
fn validate_image_current(inferior: &Inferior) -> Result<()> {
    if inferior.exec_unsupported {
        return Err(backend_error(LinuxError::UnsupportedExec));
    }
    Ok(())
}

fn validate_stopped_thread(inferior: &Inferior, pid: Pid) -> Result<()> {
    let thread = inferior
        .threads
        .get(&pid)
        .ok_or_else(|| Error::UnknownThread(debug_thread_id(pid)))?;
    if matches!(thread.state, NativeThreadState::Stopped) {
        Ok(())
    } else {
        Err(Error::NotStopped)
    }
}

fn scoped_threads(inferior: &Inferior, scope: ResumeScope) -> Result<BTreeSet<Pid>> {
    match scope {
        ResumeScope::Process(requested) => {
            validate_process(inferior, requested)?;
            Ok(inferior.threads.keys().copied().collect())
        }
        ResumeScope::Thread(thread) => {
            let pid = debug_pid(thread)?;
            validate_stopped_thread(inferior, pid)?;
            Ok(BTreeSet::from([pid]))
        }
    }
}

#[cfg(test)]
mod tests;
