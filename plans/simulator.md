# Deterministic Simulation

uscope's hardest bugs come from orderings: a thread leaves its stop between
two ptrace requests, a fork event races a pause, SIGKILL lands while a step
unwinds. Real-kernel tests meet such an ordering once in hundreds of runs,
and never on demand. The simulator runs the real debugger against a
simulated kernel and CPU, so that one 64-bit seed names one complete,
reproducible debugging session. Sweeping seeds explores thousands of
orderings per second; replaying a seed shows the same failure every time.

The approach follows FoundationDB's and TigerBeetle's simulation testing:
the system under test is driven one step at a time by a single loop, every
source of nondeterminism is drawn from the seed, and strong checks run after
every step.

## 1. Goals and non-goals

- **Reproducible.** The same seed on the same commit produces the same run
  on any machine: the same actions, trace, and fingerprint. Seeds are not
  expected to survive code changes.
- **Fast.** A session takes milliseconds and no real sleeps.
- **Real code under test.** The real `Controller` and `DebuggerHandle`, real
  DWARF loading, unwinding, stepping, and breakpoint repair, on real
  compiler output. Only the kernel and the CPU are simulated.
- **Wrong answers, not just crashes.** The simulator knows the ground truth:
  which instructions each thread executed, which stores it made, which
  frames are live. Oracles compare the debugger's answers with it.
- **Explainable.** A failure report reads as a short story: what the program
  did, what the debugger did, which rule broke, and how to replay it.

Out of scope, and covered by real-kernel scenario tests instead:

- glibc, the dynamic loader, shared libraries, `libthread_db`, and TLS.
  `thread_db` and `glibc_tls` read `/proc` outside `LinuxTraceOps`; the
  corpus has no TLS, so sessions never reach them.
- Go, Rust, Zig, and C++ programs. The corpus is libc-free C.
- `exec`, `vfork`, signal handlers, real-time signals, and group-stops.
- Floating-point and SSE semantics. A fixed FXSAVE area is reported.
- Memory ordering. Threads interleave whole instructions, which is
  sequentially consistent; debugger races come from kernel ordering.
- Core dumps, which are deterministic already.
- The DAP adapter and the CLI, which sit above `DebuggerHandle`.

## 2. Architecture

```
                          seed
                           │
                       ┌───▼────┐
                       │Choices │  one salted stream per concern
                       └───┬────┘
                           │
┌──────────────────────────▼─────────── World: one thread, one loop ───────────────────────┐
│                                                                                          │
│  Client task ── DebuggerHandle futures ──► controller queue ◄── Waiter actor ◄───┐       │
│                                                  │                               │       │
│                                   Controller::handle_message                     │       │
│                                                  │                               │       │
│                                   SimTrace: LinuxTraceOps ──────────────► Kernel ┘       │
│                                                                           │   ▲          │
│                                                                           ▼   │          │
│                                                               CPU interpreter + memory   │
│                                                                                          │
│  Oracles run after every action. Faults are actions too.                                 │
└──────────────────────────────────────────────────────────────────────────────────────────┘
```

A run builds a world from its seed, then repeats one step until the session
ends or a check fails: list the enabled actions, let the scheduler pick
one, perform it, append it to the trace, and run the oracles.

| Action | What happens | Real counterpart |
|---|---|---|
| `Run(tid)` | A running thread executes a burst of instructions, stopping early at a trap, fault, or system call, which the kernel then handles. | CPU time |
| `Collect` | The waiter reaps one reportable status, as `waitpid(-1, __WALL \| WNOHANG)` does, and queues it if the controller's queue has room. | The waiter thread |
| `Deliver` | The controller handles the message at the front of its queue. | The controller thread |
| `Poll` | The client task runs until it waits again. | A client task |

- **Preemption points.** Every call the controller makes into `SimTrace` is
  also a chance for running threads to advance, or the waiter to reap,
  before the call takes effect. That produces the races real ptrace has: a
  thread leaving its stop between `GETSIGINFO` and `GETREGS`, or a
  sibling's `exit_group` landing while a stop is handled.
- **One queue.** The waiter and the client share the controller's bounded
  `mpsc` queue, as in production, so backpressure is real.
- **The waiter reaps.** `Collect` reaps the status before the controller
  sees it, as the real waiter does. A zombie is released then, so later
  requests about it fail with ESRCH where they would in a real session.
- **Polled by hand.** `DebuggerHandle`'s futures need no runtime: channels,
  `oneshot` replies, `broadcast` with `Lagged`, and biased `select!` all
  work under a no-op waker. `tokio::time` does not, so the client sends
  `Request::Shutdown` itself rather than calling `Debugger::shutdown`, and
  never asks for source context.

The simulator lives in `src/sim`, compiled for the crate's tests and with
the `sim` feature, which builds the `uscope-sim` binary; release builds of
`uscope` never contain it. `LinuxTraceOps` is private to the Linux backend,
so `SimTrace` and the controller facade live in
`backend/linux/sim_edge.rs`. They only translate; the semantics are in
`sim/kernel`.

| Module | Responsibility |
|---|---|
| `choices`, `swarm`, `schedule` | The seed's random streams, the run's shape, and which action comes next |
| `world`, `machine` | The step loop; what actions and preemption points reach |
| `faults` | Planned faults |
| `kernel/` | Processes, threads, signals, ptrace, system calls, debug registers, and shadow state |
| `cpu/`, `memory`, `loader` | The interpreter, copy-on-write address spaces, and golden ELF images |
| `corpus`, `facts`, `markers` | The golden programs, what binutils say about them, and the conditions their sources state |
| `client/` | The simulated user driving `DebuggerHandle` |
| `oracles`, `semantics`, `hits`, `watches`, `views`, `audit` | Checks of the debugger against ground truth |
| `marks`, `report` | Coverage marks; traces, fingerprints, and failures |
| `conformance/` | Dual-run kernel probes and CPU lockstep against the real machine |
| `tests` | The gate's seeds, the determinism check, and sabotage tests |

## 3. Determinism contract

A run is a pure function of its seed and the code.

| Hazard | Rule | Enforcement |
|---|---|---|
| Random choices | Every choice comes from `Choices`. Nothing else calls a PRNG, `getrandom`, or `RandomState`. | The fingerprint check. |
| Hash iteration | Never iterate a `HashMap` or `HashSet` where order can matter. | `clippy::iter_over_hash_type` for `for` loops; `clippy.toml` bans the iterator methods. An explicit `.into_iter()` escapes both; the fingerprint check catches it. |
| Time | Nothing in a run reads a clock. The flight recorder's capture omits timestamps. | The facade builds no waiter thread and no tokio timer. |
| Process-wide state | The session lease, stop identifiers, and the flight recorder are process-global. | Simulated controllers take `SessionLease::detached()`; `SimTrace::allocate_stop_id` counts per session; each world records under a `flight_recorder::Capture`. |
| Threads | A run uses one OS thread. | Nothing in a run spawns one. |
| Host state | A run reads nothing from the host but the golden corpus, loaded once and shared read-only. | All controller host access goes through `LinuxTraceOps`. |
| Pointer identity | Never order or key data by address. | Review. |

**Fingerprint.** Every action appends lines to the run's trace, including,
in development builds, each `SimTrace` call with its result and everything
else the controller records. The fingerprint is the trace's 64-bit FNV-1a
hash. The gate runs its first 32 seeds, then again in reverse order, and
compares fingerprints (`a_seed_always_names_the_same_run`).

## 4. Choices and the swarm

The seed is expanded with SplitMix64 into one xoshiro256** generator per
stream, salted by the stream's name: `Swarm`, `Schedule`, `Preempt`,
`Client`, `Fault`, and `Program`. Both generators are written in-tree and
pinned by test vectors, so a seed means the same thing on every toolchain.
Changing how one concern draws leaves the others' choices unchanged. Draws
are integers only: `below`, `chance`, `pick`, `weighted`, and `fill`.

**Swarm configuration.** Before the session starts, the `Swarm` stream picks
the run's shape. Swarm testing (Groce et al.) finds more bugs than always
enabling everything, because some bugs appear only when other features stay
quiet. A configuration names:

- the program, its variant, and which of its argument lists to run;
- the scheduling policy and its parameters (section 9);
- how often controller calls are preempted, and the longest `Run` burst;
- the controller queue's capacity (1, 2, 8, or 32) and the event channel's
  (2, 16, or 1,024; small ones make clients see `Lagged`);
- the client's budget of requests and launches, breakpoints added before
  the first launch, whether launches stop at entry, and whether it favors
  watching memory;
- launch, or attach to a program that ran untraced for up to 999
  instructions (a third of seeds);
- a planned fault, in half of seeds (section 9);
- debug-register behavior: faithful (half), discarding as gVisor does (one
  in eight), or contended, with two to four slots of every new thread held
  by another user (section 5).

Every failure report prints the configuration.

## 5. The simulated kernel

The kernel holds processes (thread group, address space, parent, pending
signals) and threads (registers, debug registers, run state, ptrace state,
pending signals, and at most one reportable wait status). The CPU runs a
thread's instructions; the kernel turns each outcome into what Linux would
do next and answers `SimTrace`'s requests by Linux's rules. It contains no
randomness: wherever Linux leaves an order open (which thread runs, which
status a wait returns, when an interrupt lands), it exposes the options and
the world decides.

**Rules are numbered and probed.** Each rule below is pinned by a dual-run
test in `sim/conformance/kernel.rs` (section 6), and code and tests cite
rules by ID. A behavior without a passing probe is not modeled: using it
fails the run as a model gap, so the simulator never guesses.

| ID | Rule |
|---|---|
| K-EXEC-1 | A launched program first reports a stop for SIGTRAP with `si_code` `SI_USER` from itself, at its entry point, with `orig_rax` naming `execve`. Its `comm` is its file name cut to 15 bytes. |
| K-EXEC-2 | With randomization off, a static executable loads where its image says. A static-PIE one, having no interpreter, is the first mapping in the mmap area: its whole span ends at the mmap base, `0x7ffff7fff000` while the stack limit is under 127 MiB. Each segment's file pages are mapped from the file; the rest of a segment, and a segment with no file bytes, are anonymous. |
| K-WAIT-1 | A thread has at most one reportable status. A thread woken out of a stop loses an unreported one. Ptrace requests on a thread not in a ptrace-stop fail with ESRCH. |
| K-WAIT-2 | Which ready status a wait returns is unspecified, so the scheduler chooses. One exception: a group leader's exit is reported after every other thread's. |
| K-WAIT-3 | A tracer that exits releases every thread it still traces. A running or exiting one runs on untraced, one at its exit event finishes exiting, and a traced leader that exited alone joins its process's end as if never traced. |
| K-TRAP-1 | `int3` raises SIGTRAP with `si_code` `SI_KERNEL` and `rip` after the trap byte, outside any system call (`orig_rax` is -1). A single step reports `TRAP_TRACE`. A step across `syscall` reports `TRAP_BRKPT` at the call's exit, with `orig_rax` naming the call. |
| K-SIG-1 | The tracer's `tgkill(SIGSTOP)` produces a signal-delivery stop with `si_code` `SI_TKILL` and the tracer's process as sender, whether the thread was running or stopped when it was sent. |
| K-SEIZE-1 | A seized thread runs on until an interrupt stops it. The threads and processes it creates are traced with its options, and each first stops in a `PTRACE_EVENT_STOP` of its own rather than for SIGSTOP. |
| K-INT-1 | `PTRACE_INTERRUPT` stops a running seized thread with `PTRACE_EVENT_STOP` before it runs on. One sent to a thread already stopped waits until the thread resumes, ahead of a pending signal, and the thread's next stop of any kind consumes it. |
| K-INT-2 | `PTRACE_INTERRUPT` and `tgkill` return 0 for a thread at its exit event or an unreaped zombie, and ESRCH once it is reaped. When the reap lands inside the interrupt's own window, the interrupt fails with EIO; no probe can produce that race, so it is not modeled. |
| K-EXIT-1 | `exit_group` with running siblings: every thread stops at `PTRACE_EVENT_EXIT`, with message `code << 8` and `si_code` `0x605`; the caller stops inside the call (`rax` is `-ENOSYS`, `orig_rax` names it). After `PTRACE_CONT`, each reports its exit, the leader's last. A thread ending alone with `exit` stops the same way. |
| K-EXIT-2 | Siblings held in any ptrace-stop, signal-delivery or event, are pulled out of them and stop at `PTRACE_EVENT_EXIT` too. One pulled from a clone event returns from the call on the way out. |
| K-EXIT-3 | SIGKILL from anywhere: every thread stops at `PTRACE_EVENT_EXIT` with message 9, then is reported killed by signal 9. |
| K-EXIT-4 | Once a group is exiting, a thread already at its exit stop is not released by SIGKILL. It answers `GETREGS` and memory reads, and waits for `PTRACE_CONT`. A thread at the exit stop of its own `exit` is released when its group starts exiting, by `exit_group` or SIGKILL: it finishes exiting without another stop. |
| K-EXIT-5 | A leader that exits alone stays a zombie until the last thread exits. Its `exe` link is gone, its maps read empty, ptrace requests on it fail with ESRCH, and `tgkill` still succeeds. Seizing a zombie fails with EPERM. Once the tracer reaps the last thread, the process ends and its parent reaps it. |
| K-EXIT-6 | The thread that begins to exit last, before its exit stop, starts a group exit with its own status (the kernel's `synchronize_group_exit`). Once a group is exiting, every thread reaped reports the group's status, even one that exited alone earlier with another. A leader held at its exit stop therefore changes nothing; one that begins to exit last decides the status. |
| K-CLONE-1 | A creator tracing clones stops at `PTRACE_EVENT_CLONE` inside the call (`si_code` `0x305`, message the new thread's id, `rax` `-ENOSYS`); continuing it returns the id. The new thread is traced with the creator's options, starts where the creator returns, with `rax` zero and `orig_rax` naming `clone`, and first stops for a SIGSTOP with `SI_USER` from nobody. The two stops become reportable in either order. A thread created by one not tracing clones runs untraced. |
| K-FORK-1 | A fork stops the parent at `PTRACE_EVENT_FORK` inside the call, naming the child, which leads its own group and is the forking thread's child. The child gets a copy of the parent's address space, traps included. It is traced with the parent's options and starts in a stop. |
| K-FORK-2 | A detached child runs untraced: requests on it fail with ESRCH, its parent reaps it after SIGCHLD, and a trap it executes kills it with SIGTRAP. |
| K-FORK-3 | A child whose parent exits passes to a reaper, and its exit is still the tracer's to reap first. |
| K-DR-1 | A watch hit raises SIGTRAP with `TRAP_HWBKPT` and `rip` after the instruction. DR6 changes only at debug exceptions and is stale at every other stop. |
| K-DR-2 | Single-stepping over a watched store gives one stop: `TRAP_TRACE`, with DR6 holding both the single-step bit and the watch bit. |
| K-DR-3 | New threads and fork children start with debug registers disarmed, but `PEEKUSER` of DR7 returns the creator's value. Detaching does not clear them. |
| K-DR-4 | Writing a debug-register address reserves a slot even while disabled, and can fail with ENOSPC. DR7 writes are transactional: one a slot refuses, as for an address its length misaligns, changes nothing. No slot may watch the top page of user memory. |
| K-DR-5 | `rep stos` and `rep movs` trap once per iteration that touches the watched range, with `rip` still at the instruction. Stores of the same value trap. `POKEDATA` never traps. |
| K-MEM-1 | `PEEKDATA` and `POKEDATA` ignore page protections and fail only where nothing is mapped. CPU accesses obey protections and fault with `SEGV_MAPERR` or `SEGV_ACCERR`. |

Probes compare only what does not depend on timing. Where a running thread
stops for an interrupt or the tracer's SIGSTOP depends on timing, so those
probes compare signals only; whether a child a trap kills dumps core
depends on the machine, so K-FORK-2 records only the signal.

**Tracing.** The kernel knows how the tracer traces each thread: untraced,
attached (launched, and what those threads create), or seized (attached
to, and what those threads create). A program started untraced is the child
of a launcher that reaps it, as a shell would. When the controller exits,
its tracer thread exits with it, and the kernel releases what it still
traces (K-WAIT-3). Two releases are not modeled and fail as a model gap: a
thread held in a ptrace-stop other than its exit event, and
`PTRACE_O_EXITKILL` killing live threads. The controller never leaves
either behind, and the clean-exit oracle checks the first.

**Debug registers** have four slots per thread, each backed by a hardware
breakpoint once the tracer writes its address, the DR7 the tracer last
wrote, and a virtual DR6. They behave as Linux's (K-DR-1 to K-DR-5), as
gVisor's (writes succeed and change nothing, reads give zero), or
contended: another user, as a perf session following new threads can, holds
some slots of every thread the program creates, so fewer of them take an
address (K-DR-4).

**Requests and system calls.** `kernel/ptrace.rs` has one function per
`LinuxTraceOps` request, each answering in terms of the rules above. The
system calls are those the golden runtime makes: `write` (captured as the
program's output), `exit`, `exit_group`, `clone` with the flags of a thread,
`sched_yield`, `fork`, `getpid`, `getppid`, and `wait4` for one child with
`WNOHANG`. Thread stacks are static, so `mmap` is not needed, and the
runtime polls `wait4`, so no program needs SIGCHLD delivered. Any other
request, system call, or form of one is a model gap, never a plausible
default.

## 6. Keeping the model honest

The model is only as good as our understanding of Linux. Two kinds of
conformance tests, both in the gate, tie it to the real machine.

**Kernel conformance.** One test per rule runs a short script of ptrace
operations against a golden program twice, natively through the backend's
ptrace edge and through the simulated kernel, on every variant. Both runs
record the same observations (statuses, errnos, siginfo, event messages,
places relative to symbols), which must be identical. A script waits only
for what it can observe.

**CPU lockstep.** For each single-threaded golden program and variant, a
test starts the program natively, stops it at its first instruction, copies
its registers and mappings into the interpreter, and single-steps both
together, following new threads too. After every instruction it compares
the general registers, `rip`, and the defined flags; flags an instruction
leaves undefined are masked with iced-x86's `rflags_undefined`. At a
`syscall`, the native result is copied in. The first divergence fails,
printed decoded. This validates every instruction the corpus executes.

## 7. CPU, memory, and shadow state

**Interpreter.** iced-x86 decodes the instruction at `rip`; `cpu/ops.rs`
gives each implemented instruction its meaning. A step returns its outcome
(`Completed`, `Syscall`, `Breakpoint`, `Fault`, or `Unsupported`, which is a
model gap), whether it was a call or a return, and every load and store it
made, `rep` iterations one at a time. Instructions are added only when the
corpus needs them, each covered by lockstep. The CPU knows nothing of
ptrace: the kernel applies the trap flag and the debug registers to each
outcome and its accesses.

**Memory.** An address space is a map of 4 KiB pages with protections.
Pages loaded from a golden image are shared by every session and copied on
first write; fork copies the page map, not the pages. CPU accesses obey
protections; ptrace accesses do not (K-MEM-1).

**Loader.** `loader.rs` maps the `PT_LOAD` segments of a static or
static-PIE executable (K-EXEC-2), builds the initial stack (arguments, an
empty environment, and a minimal auxiliary vector with `AT_RANDOM` from
the seed), and sets `rip` to the entry point. Randomization is off, as
uscope launches programs. A static-PIE runtime relocates itself.

**Shadow state** is what threads really did, kept where the debugger cannot
see or change it (`kernel/shadow.rs`, `kernel/watching.rs`):

- Each thread's shadow call stack holds every call not yet returned from:
  its return address, the slot it was pushed to, and an identifier for the
  activation it began. A new thread begins with none of its creator's
  calls. A return anywhere but the top call's return address marks the
  shadow lost, and no oracle then judges that thread.
- Each thread counts the instructions it completed, `syscall` among them;
  a trap is not one.
- While the client steps a thread, the kernel records each instruction it
  completed no deeper than where the step began, with its depth and
  activation.
- Per thread since the last stop, the accesses to watched ranges, whether
  an armed slot covered each, and whether a store left the bytes as they
  were.
- Every execution of the program's own instruction where a user
  breakpoint is certainly enabled.

## 8. The golden corpus

Programs live in `tests/golden/` as sources and manifests. The pinned Nix
toolchain builds them byte for byte into `build/golden/`, and their hashes
must match the manifests, so a compiler upgrade never silently changes
what a test means.

```
tests/golden/
  rt/                     the freestanding runtime
  straight/
    straight.c            the source, with marker comments
    arguments             its runs, one argument list per line
    manifest.json         toolchain, hashes, variants, and each run's result
build/golden/             built, not checked in
  straight/
    straight-gcc-O0       one binary per variant
    ...
    facts.json            functions and line tables from binutils
```

**Runtime.** A few hundred lines of C and inline assembly, with no
futexes, TLS, or signals: `_start` with the static-PIE self-relocator,
`exit` and `exit_group`, `write` and `print_u64`, spin barriers that yield
with `sched_yield`, threads (`rt/thread.c`: `rt_spawn` makes a raw `clone`
on a stack whose top holds a zero return address, and `rt_thread_start` is
marked outermost in its CFI), and processes (`rt/process.c`: `rt_fork`, and
`rt_wait`, which polls `wait4` and yields). The programs also run natively,
which the conformance tests rely on.

**Variants.** Ten per program, built with `-static -nostdlib
-ffreestanding -fno-stack-protector -fcf-protection=none -g` and
vectorization off: GCC and Clang at `-O0` and `-O2`, each with and without
frame pointers, and two static-PIE builds (`gcc-O2-pie` without frame
pointers, `clang-O0-pie` with them). Every variant names its frame-pointer
choice, since the toolchain keeps frame pointers unless told otherwise.

**Programs.**

| Program | Exercises |
|---|---|
| `straight` | Loops, calls, recursion, and inlining in one thread. |
| `threads WORKERS ENDING` | Workers on raw `clone`, barriers, and shared counters, ending with the main thread exiting the group (`main`), the last worker exiting the group while others may still be exiting (`worker`), or the main thread exiting alone first (`leader`, where every thread exits 3, since the last to begin exiting decides the status). |
| `racing-exit WORKERS ROUNDS` | Every thread calls `tick` until worker 0 exits the group. |
| `frames ROUNDS` | Calls through a table of function pointers, recursion, a tail call, and hand-written assembly without line information: `bare_call` without CFI, `scribbled_call`, which overwrites its own return address while it calls back into C, and `orphan_spawn`, whose thread begins mid-function on a stack topped by a zero return address. |
| `fork ROUNDS MODE` | A child per round that redoes its parent's work in its own memory and exits 0 when it agrees, so a child released with a trap in place dies of SIGTRAP and its parent's output shows it. Mode 1 also leaves an orphan that outlives its parent; mode 2 also forks from a worker thread. |
| `stores ROUNDS WORKERS` | Global stores, same-value stores, `rep stos` and `rep movs`, four neighbouring words, and stores from several threads. |
| `data` | Records, arrays, and pointers, visited through pointers into a table. |
| `containers` | A vector, a linked list, and an open-addressed table, which `containers.views` presents; odd rounds leave the list cyclic. |

**Build and integrity.** `just build-test-programs`, which the gate and the
simulator recipes run first, runs `scripts/golden.sh build`. It rebuilds a
program whose inputs (sources, manifest, arguments, the script, and the
toolchain's versions) changed, writes its `facts.json`, and fails unless
every binary's hash, every source's hash, and what each run prints and
returns match the manifest. `just golden-record NAME` rebuilds one program
and rewrites its manifest after a deliberate change, committed on its own.
Recording runs every variant with each argument list, requires them to
agree, and runs each 20 more times: a program's behavior must not depend on
scheduling, or transparency would fail sessions for nothing.

**Facts.** `facts.json` holds, for every variant, its functions from `nm`,
its line table rows from `readelf --debug-dump=decodedline`, the addresses
`--debug-dump=rawline` marks `epilogue_begin`, whether it was optimized,
and how many inlined calls its DWARF describes. Coming from GNU binutils
rather than uscope, they let the stepping oracle check uscope against
another implementation. Rows at one address collapse as gdb collapses them,
and a row never describes code past the start of another function (GCC's
last row before hand-written assembly runs on through it).

**Markers.** A condition on a line's variables is written beside the line,
where the source hash covers it:

```c
total += square(index); // MARK: total == (index - 1) * index * (2 * index - 1) / 6
visit(item); // MARK: index < 4 // EXPECT: item == &items[index]
```

A `MARK` joins comparisons of integer expressions over variables in scope
with `&&` (`markers.rs`). An `EXPECT` is written in uscope's expression
language, which only the debugger evaluates.

## 9. Faults and scheduling

Faults are things the real world does to a session, each justified by a
rule or real behavior; the simulator never injects a failure Linux cannot
produce.

| Fault | Justification |
|---|---|
| SIGKILL from outside: at a chosen action (1 to 300) while a program runs; before the Nth call into the kernel (1 to 100) takes effect; or right after the first or second clone or fork | Any process can be killed (K-EXIT-3). |
| A sibling's `exit_group` while a stop is handled | The programs exit their own groups, and preemption points land those exits between any two controller calls (K-EXIT-1, K-EXIT-2). |
| Seizing a thread that finished exiting fails with EPERM | Attaching after an untraced run meets threads at every stage of exiting (K-EXIT-5). |
| Debug-register writes discarded, or failing with ENOSPC | gVisor, and slots held by perf (K-DR-4). |
| Tiny queue and event capacities | Legal configurations of the real channels. |
| The client pipelines requests and sends stale `StopId`s | Real clients race their own requests. |

The EIO an interrupt can meet (K-INT-2) is not injected: no probe can
justify it. Real-kernel stress runs cover the controller's handling.

**Plans.** Half of seeds plan one SIGKILL, aimed at a moment where bugs are
likely rather than left to uniform chance; a program that forks aims at a
fork as often as at any other moment. A plan that never fires is named in
the report, and a sweep prints, per kind, how many sessions that planned it
saw it fire.

**Preemption.** With a per-seed chance (0, 2, 20, or 60 percent), each
call the controller makes into the kernel is preceded by one to three
actions: a running thread executing up to 8 instructions, or the waiter
reaping. Their trace lines appear among the controller's own, marked
`preempt:`.

**Scheduling.** Half of seeds use a random walk: an action kind by the
swarm's weights, then one action of that kind. The other half use PCT
(Burckhardt et al., 2010) with 0 to 3 change points among the first 64,
512, or 4,096 actions. The waiter, the controller, the client, and each
thread are actors with priorities. A thread that yields drops below every
other actor, or a thread spinning on a barrier would starve the one it
waits for.

**Coverage marks** (`marks.rs`) count interesting states a run reached:
two threads at breakpoints in one stop, a leader exiting alone, a hit
declined by its condition, a released fork child whose parent had exited,
each kind of SIGKILL, and so on. The gate requires every mark to be reached
across its fixed seeds, which proves the sweep reaches what it claims to
test. A new feature or fault adds marks.

## 10. Oracles

Each oracle states one rule and, when it fails, cites both sides of the
disagreement. **An oracle is never loosened to make a run pass.** One that
is wrong is corrected in a change of its own that says why, with a unit
test. A new oracle gets a sabotage test in `sim/tests.rs`: the kernel or
CPU lies in the way the oracle exists to catch, and the gate's seeds must
catch it.

Breakpoints and watchpoints are judged only in the process the controller
debugs, once it has finished launching or attaching to it: before then they
are not in place, and once it detaches or exits they are gone. Once a
process is ending as a whole (killed, or exiting its group), its threads
leave their stops whatever the debugger last published, and the controller
restores nothing in it: the all-stop, ownership, and semantic oracles skip
it, a trap reported in it may go uncounted, and the client accepts any
failed request about it.

**Protocol** (the client and the event auditor):

- Every request is answered, and run-control requests are acknowledged
  before their stops are published.
- Revisions never decrease, and events share the revision of the state
  change that produced them; stop identifiers only increase.
- A request with a stale `StopId` is refused and changes nothing; so does a
  refused step.
- A memory read of code shows the program's bytes, never a planted trap.
- Every `ThreadExited` and `InferiorExited` reports the status the kernel
  reported. A leader that exits while the debugger knows other threads
  live is reported at its exit event, with the code it passed to `exit`.
- No stop reports `Exception` or `Unclassifiable` unless something outside
  killed the process: no golden program raises a signal.

**Ground truth** (the world, after every action):

- *All-stop:* while a stop is published, every thread of the inferior is in
  a ptrace-stop or a zombie, and the controller's threads are exactly the
  stopped ones and those zombies.
- *Code integrity:* every byte of code equals the image's, or is `0xcc`
  where the controller says it installed a trap over the byte it
  remembers.
- *Site ownership:* every site has an owner, only the active execution owns
  plan sites, and a published stop ended every plan.
- *Clean release:* a fork child or attached program the debugger released
  holds no byte the debugger planted.
- *Clean exit:* the controller never exits holding a thread in a stop, which
  Linux would leave stopped forever, and when the session ends no process
  remains, not even a zombie.
- *Liveness:* a run fails as stuck when nothing can happen while the client
  waits, when the controller keeps running after answering a shutdown, or
  after two million actions.

**Breakpoint accounting:**

- *Unseen hits:* from the client's add reply until it asks to remove the
  breakpoint, no thread executes the program's own instruction at its
  address except to step over the trap it just reported there.
- *Hit counts, per arrival:* handling one message counts at most the trap
  it reports. A thread's arrival at a trap counts one hit for every user
  breakpoint owning the site then. A repeat of the arrival (the same thread
  at the same address, having executed nothing since, as when a signal
  interrupts its step over the trap) counts nothing for a breakpoint that
  counted it, and at most one for one that came to the site since: whether
  a thread resumed there ever ran, a debugger can only guess. A new
  inferior starts every count again. A trap heard once the controller has
  been asked to shut down is no hit: the controller kills a launched
  process, and releases an attached one with the thread rewound to execute
  the instruction untraced.
- *Ownership:* while a stop is published, every breakpoint the client was
  told exists owns an installed site at each of its locations.
- *Conditions* (`hits.rs`, judged by the client at each stop): every hit
  counts. A hit stops when the hit condition accepts its number, its
  condition holds or fails to evaluate, and the breakpoint logs no message;
  one with a message logs instead. Between two stops, each counted hit that
  did not stop must be one some policy lets decline, each stop one some
  policy allows, and the messages logged between those every policy
  requires and those any allows. The client knows a condition's value only
  where it can tell without the debugger: a constant, or a marker's
  condition or its negation at the start of the marker's line in
  unoptimized code. A policy changed while the program runs applies from a
  hit the client cannot know, so every policy since the last stop counts.

**Watch accounting** (`watches.rs`): no thread accesses a watched range
unless an armed slot covers the access, and none runs on from an access to
a watch on stores or on any access without a stop published since. At each
new stop, a thread that accessed a watch on stores or on any access reports
a hit on it; a watch on changes reports the thread that stored when it
alone stored and the bytes differ from those at the last stop, and no
thread when they are as they were. Every hit shows the bytes at the last
stop and now, and a thread reports no hit on a watch it did not access.

**Semantic** (`semantics.rs`, judged by the world after each poll, against
shadow state and facts). At each new stop the client reads the selected
frame's variables and the backtrace of every stopped thread. Nothing is
judged once its stop is over or its process is ending.

- *Backtrace:* the physical frames are the thread's `rip` and then the
  return addresses of its calls, innermost first. A backtrace may stop
  early only with a termination other than `Complete`. Where the program
  overwrote a return address, the frame may show what the slot holds, as
  the last frame, never as a complete stack.
- *Stepping:* a step whose result is `Step { kind }` left its thread where
  the kind says:
  - an instruction step completed one instruction, or, begun inside a
    system call, finished the call (K-TRAP-1);
  - stepping over a call returned to its return address in the same frame;
    over anything else, one instruction;
  - stepping out of a physical frame returned to its return address, or,
    where no line describes that, went on to the caller's first described
    instruction, further out if the caller returned first; out of an inline
    frame, it stayed in the physical frame or returned from it;
  - a source step stopped in a statement row with a line, never at an
    epilogue marker; stepping over never stopped in a callee; stepping into
    an inline frame hidden at the stop moved nothing; and a step over begun
    in code no line describes steps as stepping in does.

  In unoptimized code, which has no inlining, split functions, or calls
  turned into jumps, source steps are judged exactly too: a step within its
  frame changed line, and the thread passed no place where it had to stop,
  the start of a statement row of another line in the frame it began in,
  or in a caller of a line other than the one the caller called from,
  short of an epilogue marker it crossed.
- *Variables:* where a thread stands at the start of a row of a marker's
  line, and the debugger presents that line in its innermost frame, the
  variables shown satisfy the marker's condition. In unoptimized code every
  variable a condition names has a value.
- *Expressions:* in the same frame, a marker's condition evaluates true,
  its negation false, and its `EXPECT` true; a variable's name, or its
  address dereferenced, evaluates to what the variables view shows, from
  bytes the simulated memory or innermost registers hold where the debugger
  says it read them; `&x` is where the view says `x` lives; sums,
  differences, and products of integer variables are exact; casts keep the
  low bits; and an ill-typed expression is refused.
- *Views* (`views.rs`): a container's presentation is what its view makes
  of memory, walked exactly as `containers.views` says from the program's
  C layout, never from uscope's reading of the debug information: its
  elements or entries, each read from where the walk finds it and the same
  in pages of any size; or, for a broken container, the typed problem the
  walk ends in.

**Transparency:** the debugger never changes what the program does. What a
program wrote so far always begins what it writes undisturbed; one that
exited by itself wrote exactly that and exited as it does. A released or
detached program runs on to its own end and must reach the same result,
including what it wrote untraced before the client attached. An orphaned
fork child must exit 0.

## 11. Running the simulator

| Command | What it does |
|---|---|
| `just` | The gate, which includes the golden build, kernel and CPU conformance, 2,000 fixed seeds over every program and variant with the coverage-mark check, the determinism double run, and the sabotage tests. |
| `just sim [SECONDS]` | A sweep: random seeds on every core for SECONDS (default 30), inside `scripts/contained.sh`. Failures are grouped by signature; each group keeps its shortest run's report. |
| `just sim-seed SEED [--fingerprint F]` | Replays one seed and prints its whole trace, also written to `target/sim/SEED/trace.log`; with a fingerprint, checks the replay is the run the sweep saw. |
| `just sim-seed SEED --at STEP` | Replays to STEP and prints the state there: each thread's state, report, pending signals, and `rip`; the waiter; the controller's queue; and the client. |

Sweeps build with `[profile.sim]`: release optimizations with debug
assertions, so the flight recorder and internal checks stay on. A sweep
runs one world per worker thread, sharing only the immutable corpus, and
runs inside a memory-capped `systemd-run` scope so a runaway session cannot
take the machine. The routine is in AGENTS.md: `just all` sweeps for 30
seconds before every commit, and lifecycle, run-control, attach,
concurrency, model, and oracle changes sweep for ten minutes before merging.

## 12. Failures

| Kind | Meaning | Response |
|---|---|---|
| Debugger | An oracle disagreed with the debugger, or the debugger hung or panicked. | Write a test outside the simulator that fails first, then fix. |
| Model gap | The program or controller used something not modeled: an instruction, system call, ptrace request, or errno. | Probe the real behavior, add a rule with its conformance test, then model it. |
| Simulator | The simulator panicked or broke its own invariant. | Fix the simulator. |

A report names the kind, seed, program, swarm configuration, failing check
and action, a planned fault that never fired, the trace's last lines with
the controller's flight recording interleaved, and the replay command with
the run's fingerprint.

**Seeds go stale.** A seed names a run only for the commit it ran on, so
seeds are never kept as tests: every bug a seed finds becomes a test in the
ordinary suites.

**Grouping.** A sweep groups failures by signature: the kind, the check, and
the message with its numbers, and any dump of a value from the first `{` or
`[` on, left out (`Failure::signature`). Runs that meet one bug differ in
thread ids, addresses, and counts, not in the words around them; two bugs
that trip one oracle usually differ in those words. Each group reports its
count and its shortest run, the smallest seed breaking ties.

There is no shrinker. Reports name the oracle, the action, and both sides
of the disagreement, the cause has been within a few dozen lines of the
trace's end, and `--at STEP` shows the whole state anywhere; a sweep meets
a common failure many times and keeps the shortest. A shrinker would
require every random choice to make zero its simplest option, a rule whose
breakage fails silently.

## Glossary

- **Action:** one atomic thing the world does in a step.
- **Fingerprint:** the hash of a run's trace. Equal fingerprints mean
  identical runs.
- **Model gap:** something uscope or a program does that the simulator does
  not model, reported as such, never guessed.
- **Oracle:** a check of the debugger's behavior against the simulation's
  ground truth.
- **PCT:** probabilistic concurrency testing. Actors get random priorities
  that change at a few random points, which finds ordering bugs of small
  depth with known probability.
- **Preemption point:** a `SimTrace` call at which other actors may act
  before the call takes effect.
- **Signature:** what failures with one cause share, by which a sweep groups
  them.
- **Swarm configuration:** the per-seed choice of which features and faults
  are active, and how intensely.
