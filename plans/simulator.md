# Deterministic Simulation

Status: P0 through P5 done, 2026-10-05 (section 17).

uscope's hardest bugs come from orderings: a thread leaves its stop between
two ptrace requests, a fork event races a pause, SIGKILL lands while a step
unwinds. Real-kernel tests meet such an ordering once in hundreds of runs,
and never on demand. This plan describes a simulator that runs the real
debugger against a simulated kernel and CPU, so that one 64-bit seed names
one complete, reproducible debugging session. Sweeping seeds explores
thousands of orderings per second; replaying a seed shows the same failure
every time.

The approach follows FoundationDB's and TigerBeetle's simulation testing:
the system under test is driven one step at a time by a single loop, every
source of nondeterminism is drawn from the seed, and strong checks run after
every step.

## 1. Goals

- **Reproducible.** The same seed on the same commit produces the same run
  on any machine: the same actions in the same order, the same trace, the
  same fingerprint. Seeds are not expected to survive code changes.
- **Fast.** A session takes milliseconds of wall time and no real sleeps.
  A one-minute sweep runs hundreds of thousands of sessions across all cores.
- **Real code under test.** The real `Controller`, the real `DebuggerHandle`
  API, real DWARF loading, unwinding, stepping, and breakpoint repair, all
  working on real compiler output. Only the kernel and the CPU are simulated.
- **Wrong answers, not just crashes.** The simulator knows the ground truth:
  which instruction each thread executed, which stores it made, which frames
  are live. Oracles compare the debugger's answers with that truth.
- **Explainable.** Each module does one thing and can be read on its own.
  Every failure report reads as a short story: what the program did, what
  the debugger did, which rule broke, and the command that replays it.

## 2. Non-goals

These are out of scope until a later plan says otherwise. Real-kernel
scenario tests keep covering them.

- glibc, the dynamic loader, shared libraries, `libthread_db`, and TLS.
- Go, Rust, Zig, and C++ programs. The corpus is libc-free C and assembly.
- `exec`, `vfork`, signal handlers, real-time signals, and group-stop caused
  by signals from outside the debugger.
- Floating-point and SSE semantics beyond the moves compilers emit for
  copies. A fixed FXSAVE area is reported.
- Memory ordering. Threads interleave one whole instruction at a time, which
  gives sequential consistency. Debugger races come from kernel ordering,
  not from the program's memory model.
- Core dumps, which are already deterministic and covered by checked-in
  fixtures.
- The DAP adapter and the CLI. They sit at the debugger's edge, above
  `DebuggerHandle`. The simulator drives `DebuggerHandle` directly; DAP keeps
  its scenario and traffic-replay tests.

## 3. What this design keeps from earlier work

- **The controller seam** (merged as `5e939d9`): `Controller::handle_message`
  serves one message at a time, the waiter thread is optional, and every
  host access in controller paths goes through `LinuxTraceOps`. This is the
  foundation the simulator plugs into.
- **Verified kernel facts.** The ptrace behaviors probed on Linux 7.1 while
  building exits, watchpoints, and the v1 simulator become the simulated
  kernel's rules (section 8). Each must be pinned by a conformance test
  before it is modeled.
- **Ideas, not code, from the unmerged v1 simulator** (`0615a3f1`). v1
  found the 13 bugs fixed on `next` in `67b4759`..`bf77646`, which proves the
  approach. Its code is not ported. It was hard to follow, being large kernel
  and world modules with interleaved concerns. Its client bypassed
  `DebuggerHandle`, and its corpus was generated rather than compiled. It
  worked around process-wide state with thread-local switches. Porting its
  fixes also showed that two of its tests were vacuous or circular. The ideas
  worth keeping are listed where they are used: salted random streams,
  preemption points inside trace calls, the invariant list, and swarm
  configuration.
- **From jetstream's oracle:** a separate random stream per concern; faults
  planned as "the Nth matching event" with a check that every planned fault
  fired; and a trace whose determinism is itself tested.
- **madsim, turmoil, and shuttle are not used.** madsim controls only the
  thread its executor runs on, panics when a thread is spawned, and runs
  `spawn_blocking` inline. uscope's nondeterminism lives below the controller
  and waiter threads, which none of these tools can schedule. uscope's own
  seam already gives what a simulation needs: a single-threaded step function
  over controller messages.

## 4. Architecture

```
                          seed
                           │
                       ┌───▼────┐
                       │Choices │  one salted stream per concern
                       └───┬────┘
                           │
┌──────────────────────────▼─────────── World: one thread, one loop ───────────────────────┐
│                                                                                          │
│  Client tasks ── DebuggerHandle futures ──► controller queue ◄── Waiter actor ◄──┐       │
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
ends or an oracle fails:

1. List the enabled actions.
2. Let the scheduler pick one, using the seed.
3. Perform it and append a line to the trace.
4. Run the oracles.

| Action | What happens | Real counterpart |
|---|---|---|
| `Run(tid, n)` | A running thread executes up to `n` instructions, stopping early at a trap, fault, or syscall, which the kernel then handles. | CPU time |
| `Collect` | The waiter reaps one reportable status, as `waitpid(-1, __WALL \| WNOHANG)` does, and queues it if the controller's queue has room. | The waiter thread |
| `Deliver` | The controller handles the message at the front of its queue. | The controller thread waking |
| `Poll(task)` | One client future is polled. | A client task |
| `Fault(kind)` | Something outside the debugger acts, such as an external SIGKILL. | The rest of the machine |

**Preemption points.** Every call the controller makes into `SimTrace` is
also a chance for running threads to advance before the call takes effect.
That is how the simulator produces the races real ptrace has: a thread
leaving its stop between `GETSIGINFO` and `GETREGS`, or a sibling's
`exit_group` landing while a stop is handled. How often and how far threads
advance is drawn from the `Preempt` stream.

**One queue, as in production.** The real waiter and the clients share the
controller's bounded `mpsc` queue. The simulator keeps that queue. The
interleaving of client requests and wait statuses is a scheduling choice,
and backpressure is real.

**The waiter reaps.** `Collect` reaps the status, as the real waiter does
before the controller sees it. A zombie is released at that moment, so
later requests about that thread fail with ESRCH exactly where they would
in a real session.

## 5. Module layout

The simulator lives in the uscope crate, under `src/sim/`, compiled for the
crate's own tests and with the `sim` cargo feature: `#[cfg(any(test,
feature = "sim"))]`. The gate's simulator tests therefore need no feature
flag, and release builds of `uscope` never contain the simulator. The
feature builds the `uscope-sim` binary. Being in the crate lets the
simulator reach the controller through one narrow facade without widening
the public API. Compiling it adds about 0.1 s to the gate.

As built in P4, with sizes in lines:

| Module | Responsibility | Lines |
|---|---|---|
| `sim/choices.rs` | Seed expansion, the PRNG, streams | 246 |
| `sim/swarm.rs` | The run's shape, chosen before it starts | 145 |
| `sim/schedule.rs` | The random walk and PCT | 222 |
| `sim/faults.rs` | Fault plans | 114 |
| `sim/world.rs` | The step loop, delivery, checks after each action | 1,077 |
| `sim/machine.rs` | What actions and preemption points reach: running threads, the waiter, faults | 308 |
| `sim/audit.rs` | The event auditor | 165 |
| `sim/kernel/mod.rs` | Process and thread tables, run states, traps | 1,118 |
| `sim/kernel/processes.rs` | Starting, forking, and reaping processes; the tracer's exit | 384 |
| `sim/kernel/ptrace.rs` | Ptrace requests and their errnos | 319 |
| `sim/kernel/signals.rs` | Signal delivery, group exits, and the exit paths | 312 |
| `sim/kernel/syscalls.rs` | `write`, `sched_yield`, `clone`, `fork`, `wait4`, `getpid`, `getppid`, `exit`, and `exit_group` | 216 |
| `sim/kernel/debug_regs.rs` | Debug registers in their three behaviors | 241 |
| `sim/kernel/watching.rs` | The accesses to watched memory, and whether a slot covered each | 157 |
| `sim/kernel/shadow.rs` | Shadow call stacks, and where a stepping thread went | 111 |
| `sim/cpu/mod.rs` | Registers, decoding, outcomes, calls and returns | 357 |
| `sim/cpu/ops.rs` | Instruction semantics | 534 |
| `sim/cpu/flags.rs` | Status flags and conditions | 179 |
| `sim/memory.rs` | Copy-on-write address spaces and protections | 428 |
| `sim/loader.rs` | Golden ELF images, static or static-PIE, and the initial stack | 354 |
| `sim/corpus.rs` | Loads the golden programs, their manifests, facts, and markers | 255 |
| `sim/facts.rs` | What binutils say about each binary's lines and functions | 266 |
| `sim/markers.rs` | Conditions on variables, from source comments | 317 |
| `sim/client/mod.rs` | The client that drives `DebuggerHandle`: the session loop and lifecycle | 753 |
| `sim/client/breakpoints.rs` | The client's breakpoints, conditions, and watchpoints | 495 |
| `sim/client/stops.rs` | What the client inspects at a stop: backtraces, steps, variables | 272 |
| `sim/oracles.rs` | Ground-truth checks | 654 |
| `sim/semantics.rs` | The backtrace, stepping, and variables oracles | 754 |
| `sim/hits.rs` | The breakpoint-conditions oracle | 345 |
| `sim/watches.rs` | The watch-accounting oracle, at each stop | 133 |
| `sim/marks.rs` | Coverage marks | 218 |
| `sim/report.rs` | Traces, fingerprints, failures | 128 |
| `sim/conformance/tracee.rs` | The dual-run harness: one script, native and simulated | 802 |
| `sim/conformance/{cpu,kernel}.rs` | Lockstep and the kernel's rules (section 9) | 1,479 |
| `sim/tests.rs` | The gate's seeds, determinism, and sabotage tests | 192 |
| `src/bin/uscope-sim.rs` | Sweep and replay commands | 240 |
| `backend/linux/sim_edge.rs` | `SimTrace`, the simulated waiter, the controller facade, and ground truth | 872 |
| `backend/linux/native_tracee.rs` | The real traced process conformance tests drive | 506 |

The client, the world, and the oracles grew past their goals with the
multi-threaded and semantic checks; each still reads as one concern. In
P4 the client grew by half, with conditions, watches, and attach, and
split along the seams P3 named: its breakpoints and watchpoints, and its
inspection of stops. The kernel's tables and the world's loop are the
next candidates; the kernel already moved processes, debug registers, and
watching into modules of their own.

`SimTrace` lives in the facade rather than in `sim/`: `LinuxTraceOps` is
private to the Linux backend, so its implementation must be too. It only
translates; the kernel's semantics live in `sim/kernel`.

Size goals are reading budgets, not hard limits. A module that grows well
past its goal is split along a seam a reader would recognize.

## 6. Determinism contract

A run is a pure function of its seed and the code. These rules keep it so,
and each one is enforced rather than hoped for.

| Hazard | Rule | Enforcement |
|---|---|---|
| Random choices | Every choice comes from `Choices`. Nothing else in the run calls a PRNG, `getrandom`, or `RandomState`. | Review; the fingerprint check catches leaks. |
| Hash iteration order | Never iterate a `HashMap` or `HashSet` where order can affect behavior. Lookups are fine. | `clippy::iter_over_hash_type` denied crate-wide for `for` loops; `clippy.toml` bans the iterator methods (`iter`, `keys`, `values`, `drain`, set operations). An explicit `.into_iter()` still escapes both; the fingerprint check catches what the lints miss. |
| Time | Simulated code reads no clock. The flight recorder's capture sink omits timestamps. Waiter backoff and client timeouts never run in a session. | The sim's facade constructs no waiter thread and no tokio timer. |
| Process-wide state | The session lease, `NEXT_STOP_ID`, and the flight recorder's ring are process-global. Sessions on parallel worker threads must not share them. | Simulated controllers take `SessionLease::detached()`; stop IDs come from `LinuxTraceOps::allocate_stop_id`, which `SimTrace` counts per session; each world records under a `flight_recorder::Capture` (section 16). |
| Threads | A run uses exactly one OS thread. | Nothing in the sim spawns threads. The controller under simulation never calls `spawn_waiter`'s thread path. |
| Host files and `/proc` | Simulated sessions read no host state except the golden corpus, loaded once and shared read-only. | All controller host access already goes through `LinuxTraceOps` (`5e939d9`). |
| Pointer identity | Never order or key data by memory address. | Review. |

**Fingerprint.** Every action appends a line to the run's trace. In
development builds that includes each `SimTrace` call with its result,
through the existing `Recorded` wrapper, and everything else the
controller records, captured by `flight_recorder::Capture`. The
fingerprint is a 64-bit FNV-1a hash of the trace. The gate runs the first
32 seeds, then runs them again in reverse order and compares fingerprints,
so a determinism leak fails `just` rather than surfacing as an
unreproducible sweep failure.

## 7. Choices: the only source of randomness

- **Generator.** The seed is expanded with SplitMix64 into one xoshiro256**
  state per stream. Both are written in-tree, about 40 lines, and pinned by
  golden-value tests, so a seed means the same thing on every toolchain and
  dependency version.
- **Streams.** Each concern draws from its own stream, salted by the stream's
  name: `Swarm`, `Schedule`, `Preempt`, `Client`, `Fault`, and `Program`.
  Changing how the client draws leaves the scheduler's decisions unchanged,
  which keeps seeds readable while debugging.
- **One API.** `Choices::below(stream, n)`, `chance(stream, per_mille)`, and
  `pick(stream, &[T])`. Draws are integers only: no floats and no hashing.
- **Recorded draws.** In a replay, every draw can be logged with its stream
  and the value chosen. Draws are not stored as a tape for shrinking; P5
  decided against it (section 15).

**Swarm configuration.** Before the session starts, the `Swarm` stream
picks the run's shape. Swarm testing (Groce et al.) finds more bugs than
always enabling everything, because some bugs only appear when other
features stay quiet. A seed's configuration includes:

- the program and its compiled variant, and the program's arguments;
- the scheduling policy: random walk, or PCT with 1 to 3 priority change
  points;
- the relative weights of `Run`, `Collect`, `Deliver`, and `Poll`;
- the controller queue capacity (1, 2, 8, or 32) and the event broadcast
  capacity (2, 16, or 1,024; small capacities make clients see `Lagged`);
- client pipelining depth (1 to 4), the request mix, and whether the client
  deliberately sends stale `StopId`s;
- launch, or attach after the program runs untraced for a while;
- each fault kind on or off, and its intensity (section 13);
- debug-register behavior: faithful, discarding (as gVisor does), or
  contended (slots taken by other users, so writes fail with ENOSPC).

The configuration is printed with every failure.

## 8. The simulated kernel

The kernel holds processes (thread group, address space, children, parent)
and threads (registers, debug registers, scheduling state, ptrace state,
pending signals, and at most one reportable wait status). The CPU runs a
thread's instructions. The kernel turns every outcome into what Linux would
do next, and answers `SimTrace`'s requests by Linux's rules.

**Rules are numbered, written down, and probed.** Each rule has an ID, a
one-sentence statement, the code that implements it, and a conformance test
against the real kernel (section 9). A behavior without a passing probe is
not modeled. Using it fails the run as a model gap, so the simulator never
guesses. The initial rules come from the verified facts:

| ID | Rule |
|---|---|
| K-EXEC-1 | A launched program first reports a stop for SIGTRAP with `si_code` `SI_USER` from itself, at its entry point, with `orig_rax` naming `execve`. Its `comm` is its file name cut to 15 bytes. |
| K-EXEC-2 | With randomization off, a static executable loads where its image says. A static-PIE one, having no interpreter, is the first mapping in the mmap area: its whole span ends at the mmap base, `0x7ffff7fff000` while the stack limit is under 127 MiB. Each segment's file pages are mapped from the file; the rest of a segment, and a segment with no file bytes, are anonymous. |
| K-WAIT-1 | A thread has at most one reportable status. A thread woken out of a stop loses an unreported one. Ptrace requests on a thread not in a ptrace-stop fail with ESRCH. |
| K-WAIT-2 | Which ready status a wait returns is unspecified, so the scheduler chooses. One exception: a group leader's exit is reported after every other thread's. |
| K-TRAP-1 | `int3` raises SIGTRAP with `si_code` `SI_KERNEL` and `rip` after the trap byte, outside any system call (`orig_rax` is -1). A single step reports `TRAP_TRACE`. A step across `syscall` reports `TRAP_BRKPT` at the call's exit, with `orig_rax` naming the call. |
| K-SIG-1 | The tracer's `tgkill(SIGSTOP)` produces a signal-delivery stop with `si_code` `SI_TKILL` and the tracer's process as sender, whether the thread was running or stopped when it was sent. |
| K-SEIZE-1 | A seized thread runs on until an interrupt stops it. The threads and processes it creates are traced with its options, and each first stops in a `PTRACE_EVENT_STOP` of its own rather than for SIGSTOP. |
| K-INT-1 | `PTRACE_INTERRUPT` stops a running seized thread with `PTRACE_EVENT_STOP` before it runs on. One sent to a thread that is already stopped waits until the thread resumes, ahead of a pending signal, and the thread's next stop of any kind consumes it. |
| K-INT-2 | `PTRACE_INTERRUPT` and `tgkill` return 0 for a thread at its exit event or an unreaped zombie, and ESRCH once it is reaped. When the reap lands inside the interrupt's own window, the interrupt fails with EIO; that race cannot be probed, so it is not modeled (section 13). |
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
| K-WAIT-3 | A tracer that exits releases every thread it still traces. A running or exiting one runs on untraced, one at its exit event finishes exiting, and a traced leader that exited alone joins its process's end as if never traced. |
| K-DR-1 | A watch hit raises SIGTRAP with `TRAP_HWBKPT` and `rip` after the instruction. DR6 changes only at debug exceptions and is stale at every other stop. |
| K-DR-2 | Single-stepping over a watched store gives one stop: `TRAP_TRACE`, with DR6 holding both the single-step bit and the watch bit. |
| K-DR-3 | New threads and fork children start with debug registers disarmed, but `PEEKUSER` of DR7 returns the creator's value. Detaching does not clear them. |
| K-DR-4 | Writing a debug-register address reserves a slot even while disabled, and can fail with ENOSPC. DR7 writes are transactional: one a slot refuses, as for an address its length misaligns, changes nothing. No slot may watch the top page of user memory. |
| K-DR-5 | `rep stos` and `rep movs` trap once per iteration that touches the watched range, with `rip` still at the instruction. Stores of the same value trap. `POKEDATA` never traps. |
| K-MEM-1 | `PEEKDATA` and `POKEDATA` ignore page protections and fail only where nothing is mapped. CPU accesses obey protections and fault with `SEGV_MAPERR` or `SEGV_ACCERR`. |

**Probed so far** (`sim/conformance/kernel.rs`): K-EXEC-1, K-EXEC-2, K-TRAP-1,
K-SIG-1, K-WAIT-1, K-WAIT-2 (the leader's exit is held back), K-EXIT-1 to
K-EXIT-6, K-CLONE-1, and K-MEM-1, each in its multi-threaded form where
one exists. Two P2 probes corrected the model before any session used it:
K-EXIT-4 had said a thread at its exit stop is never released, and the
leader's status had been modeled as its own until the last thread finished
exiting (K-EXIT-6). P4 added K-SEIZE-1, K-INT-1, K-INT-2, K-WAIT-3,
K-FORK-1 to K-FORK-3, K-DR-1 to K-DR-5, and two K-EXIT-5 probes: a zombie
cannot be seized, and an untraced zombie leader's process ends when the
tracer reaps its last thread. Every rule in the table now has a probe.
Where a running thread stops for an interrupt depends on timing, so the
K-INT-1 probe compares signals only; and whether a child a trap kills
dumps core depends on the machine, so K-FORK-2 records only the signal.

As built in P4, the kernel knows how the tracer traces each thread:
untraced, attached (launched, and what those threads create), or seized
(attached to, and what those threads create). An untraced program starts
as the child of a launcher that reaps it, as a shell would. When the
controller exits, its tracer thread exits with it, and the kernel
releases what it still traces (K-WAIT-3). Two releases are not modeled
and fail the run as a model gap: a thread held in a ptrace-stop other
than its exit event, and `PTRACE_O_EXITKILL` killing live threads. The
controller never leaves either behind, and an oracle checks the first
(section 12).

**Debug registers** (`sim/kernel/debug_regs.rs`) have four slots per
thread, each backed by a hardware breakpoint once the tracer writes its
address, the DR7 the tracer last wrote, and a virtual DR6. They behave in
one of three ways, chosen per seed: as Linux's (K-DR-1 to K-DR-5);
discarding every write and reading zero, as under gVisor; or contended,
with others holding some slots of every thread the program creates, as a
perf session following new threads can, so fewer of them take an address
(K-DR-4).

**Request semantics.** `sim/kernel/ptrace.rs` has one function per
`LinuxTraceOps` method. Each documents its errno outcomes in terms of the
rules above. For example, `continue_execution` on a thread that has left
its stop returns ESRCH (K-WAIT-1). Methods the controller uses but the
simulator does not support yet return a model-gap failure, never a
plausible default.

**Syscalls** modeled for the corpus runtime: `write` (captured as the
program's output), `exit`, `exit_group`, `clone` (threads, with the flags
`pthread_create` passes, without the TLS and tid bookkeeping), and
`sched_yield`. Thread stacks are static arrays, so `mmap` is not needed.
P4 adds what forking programs use: `fork`, `getpid`, `getppid`, and
`wait4` for one child with `WNOHANG`, which the runtime polls, yielding,
so no program needs SIGCHLD delivered. Any other syscall, or another form
of these, is a model gap.

**The world, not the kernel, chooses.** Wherever Linux leaves an order
open (which thread runs, which status a wait returns, when a pending
interrupt lands), the kernel exposes the options and the scheduler decides.
The kernel itself contains no randomness.

## 9. Keeping the model honest

The model is only as good as our understanding of Linux. A week before this
plan, `PTRACE_INTERRUPT` would have been modeled wrong. Two kinds of
conformance tests tie the simulation to real behavior. Both run in `just`.

**Kernel conformance.** `src/sim/conformance/` holds one unit test per
rule. These are unit tests because `LinuxPtrace` is private to the crate.
Each test runs a short script of ptrace operations against a golden program
twice:

- once natively, through `LinuxPtrace` on the test thread;
- once through `SimTrace` on the simulated kernel.

Both runs record the same observations: statuses, errnos, siginfo, and
messages. The test requires them to be identical. A script synchronizes
only on what it can observe, as every test here must. A rule with no
passing dual-run test is not modeled.

**CPU conformance (lockstep).** For each single-threaded golden program and
variant, a test starts the program natively under ptrace and stops it at
its first instruction. It copies the program's registers and every mapping
into the interpreter, then single-steps both together. After every
instruction it compares the general registers, `rip`, and the defined
flags. Flags an instruction leaves undefined are masked using iced-x86's
`rflags_undefined`. At a `syscall`, the native result is copied into the
interpreter. The test fails at the first instruction that diverges, and
prints it decoded. This validates the interpreter against the real CPU for
every instruction the corpus actually executes. The corpus programs run a
few thousand instructions, so the whole suite takes about a second.

## 10. The CPU interpreter and address spaces

**Interpreter.** iced-x86, already a dependency, decodes the instruction at
`rip`. A `match` on the mnemonic dispatches to semantics grouped by family:
data movement, arithmetic and logic, shifts, control flow, string
operations, `setcc`/`cmovcc`, and the SSE moves compilers use for copies.
Each step returns an outcome:

- `Completed`;
- `Syscall`;
- `Breakpoint` (`int3`, with `rip` past it);
- `Fault { signal, code, address }`;
- `Unsupported(instruction text)`, which becomes a model-gap failure.

It also returns the step's data accesses (address, size, read or write, old
and new value), which watchpoints and the oracles use. Instructions are
added when the corpus needs them, each covered by the lockstep test. Nothing
is implemented speculatively.

**Single-step and debug exceptions.** The kernel, not the CPU, applies the
trap flag and DR0–DR7 to each step's outcome and accesses, following K-DR-*
and K-TRAP-1. The interpreter knows nothing of ptrace.

**Shadow state for oracles.** As it executes, the interpreter keeps facts
the debugger must not be able to see or change:

- each thread's shadow call stack: return addresses pushed by `call` and
  popped by `ret`;
- a per-thread count of executions at every address where a user breakpoint
  is currently enabled;
- the log of stores to watched ranges.

As built in P3, the kernel keeps the shadow state (`sim/kernel/shadow.rs`),
from the calls and returns the interpreter reports:

- Each thread's shadow call stack holds, for every call not yet returned
  from, its return address, the slot it was pushed to, and an identifier
  for the activation it began. A new thread begins with none of its
  creator's calls. A return to anywhere but the top call's return address
  marks the shadow lost, and no oracle then judges that thread.
- Each thread counts the instructions it completed, `syscall` among them.
  A trap is not an instruction completed.
- While the client steps a thread, the kernel records each instruction the
  thread completed no deeper than where the step began, with the depth and
  activation it ran in. The stepping oracle reads where the thread went
  from these.

P2's executions at user breakpoints became the unseen-hits check (section
12). As built in P4, the log of accesses to watched ranges is kept per
thread since the last stop, with whether an armed slot covered each and
whether a store left the bytes as they were (`sim/kernel/watching.rs`).
The CPU reports every load and store an instruction makes, `rep`
iterations one by one, so the kernel needs no knowledge of which
instructions access memory.

**Memory.** An address space is a map of 4 KiB pages with protections.
Pages loaded from a golden image are shared, and copied on first write, so
a session costs little more than its stack and the pages its program
writes. Fork copies the page map, not the pages. CPU accesses obey
protections; ptrace accesses do not (K-MEM-1).

**Loader.** `sim/loader.rs` maps the `PT_LOAD` segments of a static
executable, builds the initial stack (`argc`, `argv`, an empty environment,
and a minimal auxiliary vector: `AT_PAGESZ`, `AT_ENTRY`, `AT_PHDR`,
`AT_RANDOM` from the seed), and sets `rip` to the entry point. ASLR is off,
as uscope launches programs. A static-PIE image loads where K-EXEC-2 says,
just below the mmap base, and its runtime relocates itself. A segment's
pages beyond its file bytes are anonymous mappings, as Linux maps them.

## 11. The golden corpus

Programs live in `tests/golden/` as sources and manifests. Their binaries
are built with the Nix-pinned toolchain, which reproduces them byte for
byte, and must match the hashes the manifests record, so compiler upgrades
never silently change what a test means. (Until P4 the binaries were
checked in too; section 18 records why they no longer are.)

```
tests/golden/
  rt/                     the freestanding runtime: _start, syscalls, threads
  straight/
    straight.c            the source, with marker comments (// MARK: name)
    arguments             its runs, one argument list per line
    manifest.json         variants, hashes, expected behavior
build/golden/             built, not checked in
  straight/
    straight-gcc-O0       one binary per variant
    straight-clang-O2-nofp
    ...
    facts.json            functions and line tables from binutils
```

**The runtime** is a few hundred lines of C and assembly:

- `_start`, `exit`, and `exit_group`;
- `write`, and a `print_u64` helper;
- `spawn_thread(fn, arg)` using raw `clone` with a stack from `mmap`;
- spin barriers on atomics that yield with `sched_yield`;
- `fork_and_wait`;
- the static-PIE self-relocator, about 30 lines.

It needs no futexes, TLS, or signals. The programs also run natively, which
the conformance tests depend on.

**Variants.** Each program is built with GCC and Clang, at `-O0` and `-O2`,
with and without frame pointers, and as static and static-PIE executables,
where that combination teaches something. Every build uses `-static
-nostdlib -ffreestanding -fno-stack-protector -fcf-protection=none -g`.
Hand-written assembly programs cover what compilers will not produce:

- frames without CFI;
- a thread whose first frame starts mid-function, as after a raw clone;
- a frame whose return address is corrupt.

**Build and integrity.**

- `just build-test-programs`, which `just test`, the gate, and the
  simulator recipes run first, builds every program into `build/golden`
  and writes its `facts.json` there. It fails unless every binary's hash,
  every source's hash, and what each run prints and returns match the
  manifest. A source edited without re-recording, or a toolchain that
  builds different bytes, cannot go unnoticed. A program whose inputs (its
  sources, manifest, and arguments, the script, and the toolchain's
  versions) are unchanged is not rebuilt: a cold build takes about 13
  seconds, a warm one about a second.
- `just golden-record NAME` rebuilds one program and rewrites its
  manifest, after a deliberate change to its sources or the toolchain. A
  new manifest is committed on its own.

**Facts are independent of uscope.** `facts.json` comes from
`llvm-dwarfdump --debug-line` and `nm`, not from uscope's DWARF code, so the
stepping oracle checks uscope against another implementation.

**Manifest.** For each run configuration (program plus arguments), the
manifest records:

- the expected exit status and output;
- the maximum thread count;
- whether the program forks;
- variable expectations at markers, such as `total == 6 at FIBER_EXIT`,
  written in closed form.

As built in P1, a manifest records the toolchain, the hashes of the
sources and binaries, each variant's flags, and each run's arguments, exit
code, and output. A program's `arguments` file lists its runs, one argument
list per line; recording runs every variant with each and requires
them to agree. The other fields arrive with the programs that need them.
Threads interleave differently on every run, so recording runs each
binary 20 more times and fails if anything it prints or returns changes:
a program's behavior must not depend on scheduling, or transparency would
fail sessions for nothing.

**Size.** Each binary is a few kilobytes plus its DWARF. Only sources and
manifests are checked in, about 100 KB for six programs.

**Initial programs.**

| Program | Exercises |
|---|---|
| `straight` | Loops, calls, recursion, and inlining in one thread. |
| `threads` | Workers created with raw `clone`, barriers, shared counters, and workers ending by `exit` and by `exit_group`. |
| `racing-exit` | `exit_group` while siblings run and hit breakpoints. |
| `fork` | A fork child that outlives its parent, a child reaped by the parent, and forks racing breakpoint edits. |
| `stores` | Global stores, same-value stores, `rep stos`, and adjacent watched ranges. |
| `frames` | Tail calls, `-O2` frames without frame pointers, and the hand-written orphan and CFI-less frames. |

As built in P2, threads come from `rt/thread.{c,h}`: `rt_spawn` makes a
raw `clone` on a stack the caller provides, whose top holds a zero return
address so unwinders stop there, and `rt_thread_start`, marked outermost
in its CFI, runs the thread's function and exits it. `threads WORKERS
ENDING` ends with the main thread exiting the group after the workers
exit one by one (`main`), with the last worker exiting the group while
others may still be exiting (`worker`), or with the main thread exiting
alone first (`leader`). In `leader` mode every thread exits with status 3,
since the thread that begins to exit last decides the status (K-EXIT-6).
`racing-exit WORKERS ROUNDS` has every thread call `tick` until worker 0
exits the group after its own ticks.

**As built in P3.**

- *Variants.* Ten per program: GCC and Clang at `-O0` and `-O2`, each with
  and without frame pointers, all static, and two static-PIE builds
  (`gcc-O2-pie` without frame pointers, `clang-O0-pie` with them). Every
  variant names its frame-pointer choice: the Nix toolchain keeps frame
  pointers unless told otherwise, so the P1 and P2 `-O2` builds had them.
  A static-PIE build links with `-static-pie`, and `rt_start` applies its
  relative relocations before anything reads a pointer they cover.
- *Facts.* `facts.json` holds, for every variant, its functions from `nm`,
  its line table rows from `readelf --debug-dump=decodedline`, the
  addresses `readelf --debug-dump=rawline` marks `epilogue_begin`, whether
  it was optimized, and how many inlined calls its DWARF describes.
  The build writes it from the binaries it verified.
  GNU binutils stand in for `llvm-dwarfdump`, which the Nix shell lacks;
  they are as independent of uscope. Rows at one address collapse as gdb
  collapses them, the convention uscope documents too, and a row never
  describes code past the start of another function: GCC's last row before
  hand-written assembly runs on through it.
- *Markers.* A condition on a line's variables is written beside the line,
  as `// MARK: total == (index - 1) * index * (2 * index - 1) / 6`, rather
  than in the manifest: it stays next to the code it describes, and the
  source hash already covers it. Conditions compare integer expressions
  over variables in scope, joined by `&&` (`sim/markers.rs`).
- *The `frames` program.* `frames ROUNDS` calls through a table of function
  pointers (relocations in its PIE builds), recurses, and makes a call
  optimized builds turn into a jump. Its hand-written frames are top-level
  assembly in `frames.c`, without line information, rather than separate
  programs: `bare_call`, with no CFI; `scribbled_call`, which overwrites its
  own return address while it calls back into C; and `orphan_spawn`, whose
  thread begins in the middle of it, on a stack topped by a zero return
  address. The orphan thread runs `descend` under both of the others.
- *Size.* 1.2 MB, of which 364 KB is facts, for 40 binaries.

**As built in P4.**

- *The `fork` program.* `fork ROUNDS MODE` forks a child per round and
  waits for it (`rt/process.{c,h}`: `rt_fork`, and `rt_wait`, which polls
  `wait4` and yields). Mode 1 also leaves an orphan that waits for its
  parent to exit; mode 2, the default, also forks from a worker thread.
  Every child redoes its parent's work in its own copy of memory and exits
  0 when that agrees, so a child released with a trap in place dies of
  SIGTRAP, and its parent's output shows it. The parent checks that no
  child's write reached its own copy.
- *The `stores` program.* `stores ROUNDS WORKERS` stores to globals, with
  `rep stos` and `rep movs` over a buffer, to four neighbouring words,
  and from several threads. Each round first stores the bytes already
  there, which a watchpoint on stores reports and one on changes does not,
  and workers add zero to a shared counter every other round.
- *Size.* 1.8 MB built, of which 605 KB is facts, for 60 binaries. That
  neared the 2 MB budget for checking them in, so P4 stopped checking them
  in (section 18).

## 12. Oracles

Oracles are where the simulator's power lies. Each one is a small function
with a name, a one-line rule, and a failure message that cites both sides
of the disagreement. **An oracle is never loosened to make a run pass.**
When an oracle is wrong, it is fixed in a commit that explains the error.

**Protocol (checked by the client):**

- Every request is answered exactly once.
- A run-control request is acknowledged before its stop is published.
- Revisions and `StopId`s only increase.
- A request with a stale `StopId` is refused, and the refusal changes
  nothing.
- Events agree with snapshots taken right after them.
- A memory read never shows a trap byte the debugger planted.

**Ground truth (checked by the world after every action):**

- *All-stop:* a published stop means every live thread is in a ptrace-stop.
- *Code integrity:* every byte of executable memory equals the image's
  original, or is `0xcc` at a site the controller currently owns.
- *No leftover traps:* an execution plan's sites are gone once the plan
  ends.
- *Clean release:* a released fork child holds no trap byte the debugger
  planted.
- *Clean exit:* when a session ends, nothing remains traced, and no process
  the session launched is alive unless it was detached on purpose.

**Semantic (shadow state against the debugger's answers):**

- *Breakpoint accounting:* every execution of an enabled user-breakpoint
  address by a traced thread produces a reported hit, or is consumed by its
  condition or hit count. No hit is reported without such an execution. Hit
  counts equal the shadow counts.
- *Watch accounting:* reported watch hits match the stores logged to
  watched ranges. Value-change watches fire exactly when the stored value
  differs.
- *Backtrace:* the debugger's physical frames, ignoring inline frames,
  equal the shadow call stack.
- *Stepping:* after `Step { kind }`, the thread's location satisfies that
  kind's rule. The rule is computed from `facts.json` and the shadow call
  stack at the step's start, not from uscope's code.
- *Variables:* at manifest markers, the debugger's values equal the
  manifest's expectations.

**Transparency:** the debugger never changes what the program does. When a
session ends by the program exiting, its output and exit status equal the
manifest's. A detached or released program is run to completion
afterwards, and must reach the same result.

**Liveness:**

- A run fails as stuck when no action is enabled while a request or a stop
  is still owed. This is how the v1 simulator found the fork-barrier hang.
- A step budget, two million by default, ends runaway sessions.

**As built in P2.** The world checks after every action:

- *All-stop:* while a stop is published, every thread of the inferior is
  in a ptrace-stop or a zombie, and the controller's threads are exactly
  the stopped ones and those zombies.
- *Breakpoint accounting*, in three parts. Unseen hits: the kernel watches
  every address where a user breakpoint is certainly enabled, from the
  client's add reply until it asks to remove it, and no thread may execute
  the program's own instruction there except to step over the trap it just
  reported. Hit counts: when the controller handles a status reporting a
  trap the CPU executed, every user breakpoint owning that site gains
  exactly one hit; nothing else changes a count, except a launch, which
  starts them again. Ownership: while a stop is published, every
  breakpoint the client was told exists owns an installed site at each of
  its locations.
- *Thread exits:* every `ThreadExited` event reports the status the kernel
  reported for that thread, as `InferiorExited` does for the process.

Once a process is ending as a whole (killed, by the debugger or from
outside, or exiting its group), its threads leave their stops whatever the
debugger last published, and the controller deliberately restores nothing
in its address space. All-stop, ownership, and site ownership are not
checked for it, a trap reported in it may go uncounted, and the client
accepts any failed request about it.

The client also fails a run on any `Exception` or `Unclassifiable` stop of
a process not killed from outside: no golden program raises a signal.

**As built in P3.** The semantic oracles run in every session
(`sim/semantics.rs`). At each new stop the client inspects: the selected
frame's variables, and the backtrace of every stopped thread, through
`DebuggerHandle::at` for those not selected. It reports what it saw, and
each step it begins and ends, and the world judges after the poll, by the
kernel's shadow state and `facts.json`. Nothing is judged once its stop is
over or its process is ending.

- *Backtrace:* the physical frames are the thread's `rip` and then the
  return addresses of its calls, innermost first. A backtrace may stop
  early only with a termination other than `Complete`; one that is
  complete shows every call. Where the program overwrote a return address,
  the frame may show what the slot holds, as the last frame, never as a
  complete stack.
- *Stepping:* a step whose result is `Step { kind }` left its thread where
  the kind says:
  - an instruction step completed one instruction, or, begun inside a
    system call, finished the call (K-TRAP-1);
  - stepping over a call returned to its return address in the same frame;
    over anything else, one instruction;
  - stepping out of a physical frame returned to its return address, or,
    where no line describes that, went on through undescribed code to the
    caller's first described instruction; out of an inline frame, it stayed
    in the physical frame or returned from it;
  - a source step stopped in a statement row with a line, never at an
    epilogue marker; stepping over never stopped in a callee; stepping into
    an inline frame hidden at the stop moved nothing; and a step over begun
    in code no line describes steps as stepping in does.

  In unoptimized code, which has no inlining, jumps between functions, or
  split functions, source steps are judged exactly too. A step within its
  frame changed line, and the thread passed no place where the step had to
  stop: the start of a statement row of another line in the frame it began
  in, or in a caller, of a line other than the one the caller called from,
  short of an epilogue marker it crossed.
- *Variables:* where a thread stands at the start of a row of a marker's
  line, and the debugger presents that line in its innermost frame, the
  variables it shows satisfy the marker's condition. In unoptimized code,
  every variable a condition names has a value.

Each new oracle has a sabotage test: a kernel that misreports return
addresses fails backtraces, a CPU whose single steps run on fails steps,
and a kernel that misreports small numbers on the stack fails variables.

**As built in P4.** Breakpoints and watchpoints are judged only in the
process the controller says it debugs, and only once it has finished
launching or attaching to it (`Truth::established`): before then they are
not yet in place, and once the controller detaches or exits they are gone.
The kernel follows that process's accesses to watched memory, and starts
them again whenever the process the controller debugs changes.

- *Hit counts, per arrival.* P2's rule, one hit per reported trap, was
  wrong where a thread executes the same trap twice without executing
  anything else: a signal interrupts its step over the trap, and the step
  begins again from the trap. That is one arrival. A thread's arrival at
  a trap counts one hit for every user breakpoint owning the site then; a
  repeat of it, the same thread at the same address with the same number
  of instructions retired, counts nothing for a breakpoint that counted
  it, and may count it once for one that came to the site since, since
  whether a thread resumed there ever ran, a debugger can only guess. A
  new inferior, launched or attached, starts every count again, whatever
  message started it. A trap heard once the controller has been asked to
  shut down is no hit: the controller kills a launched process, and
  releases an attached one with the thread rewound to execute the
  instruction untraced, as if the breakpoint had gone first.
- *Breakpoint conditions* (`sim/hits.rs`, checked by the client at each
  stop). Every hit counts. A hit stops when the breakpoint's hit condition
  accepts its number and its condition holds, or fails to evaluate, and
  it has no log message; one with a message logs instead. Between two
  stops of a process, each counted hit that did not stop must be one that
  could decline, each stop must be one some policy allows, and the
  messages logged must lie between those every policy requires and those
  any allows. The client knows whether a condition holds only where it can
  tell without the debugger: a constant, or a marker's condition or its
  negation at the start of the marker's line in unoptimized code.
  Elsewhere either outcome is accepted. A policy changed while the program
  runs applies from a hit the client cannot know, so every policy since
  the last stop is considered.
- *Watch accounting* (`sim/watches.rs`, `sim/kernel/watching.rs`). The
  kernel follows every watch the client was told exists. No thread may
  access a watched range unless an armed slot covers the access; none may
  run on from an access to a watch on stores or on any access without a
  stop published since. At each new stop, a thread that accessed a watch
  on stores or on any access reports a hit on it; a watch on changes
  reports the thread that stored, when it alone stored and the bytes
  differ from those at the last stop, and no thread when they are as they
  were. Every hit shows the bytes at the last stop and now, and a thread
  reports no hit on a watch it did not access.
- *Clean release:* a fork child or attached program the debugger released
  holds no byte the debugger planted. The `fork` program makes this
  observable too: a child released with a trap dies of SIGTRAP, and its
  parent's output differs.
- *Transparency* follows a released program to its own end, and covers a
  program that ran untraced before the client attached: what it wrote
  before and after together is what it writes undisturbed.
- *Clean exit:* the controller never exits holding a thread in a stop,
  which Linux would leave stopped forever (section 8). *Liveness:* it
  never keeps running after it answers a shutdown.

The new oracles each have a sabotage test: a kernel that misreports small
numbers on the stack fails breakpoint conditions; debug-register writes
that reach only a copy for threads other than a process's first, and such
threads taking no debug exception, both fail watch accounting.

## 13. Faults

Faults are things the real world can do to a debugging session. Each one
carries the kernel rule or real behavior that justifies it. The simulator
never injects a failure Linux cannot produce.

| Fault | Justification |
|---|---|
| External SIGKILL at a random step | Any process can be killed (K-EXIT-3). |
| SIGKILL near a clone, fork, or exec event, or between two trace calls | The same, aimed where v1 found most bugs. |
| A sibling's `exit_group` while a stop is handled | Programs exit whenever they like (K-EXIT-1, K-EXIT-2). |
| A thread reaped inside `PTRACE_INTERRUPT`'s window, returning EIO | K-INT-2, observed in stress runs. Not modeled: no probe can produce it on demand. |
| Seizing a thread that is just exiting fails with EPERM | K-EXIT-5. |
| Debug-register writes discarded, or failing with ENOSPC | gVisor, and slots held by perf (K-DR-4). |
| Tiny queue and broadcast capacities | Legal configurations of the real channels. |
| The client pipelines requests and resends stale `StopId`s | Real clients race their own requests. |

**Plans.** The swarm turns each fault kind on or off per seed. Enabled
faults fire by plan: "the 3rd clone event", "50 steps after the first
stop". This aims faults at interesting moments far more often than uniform
chance would. Every planned fault must fire, or the run reports it as
unfired. A sweep where faults silently never happen is useless.

As built in P2, half the seeds plan one SIGKILL from outside: at the
first action from a chosen one (1 to 300) while a program runs; between
two ptrace requests, before the Nth call into the kernel (1 to 100) takes
effect; or, for programs that create threads, right after the first or
second clone. A report names a plan that never fired, and a sweep prints,
for each kind, how many of the sessions that planned it saw it fire
(about half: many sessions end first). A sibling's `exit_group` is not
injected: `threads` and `racing-exit` exit their groups themselves, and
preemption points let the exit land between any two of the controller's
calls.

As built in P4:

- A program that forks may plan its SIGKILL right after its first or
  second fork, as often as at any other moment, since forks are rarer than
  clones.
- Debug registers behave faithfully in half the seeds, discard every
  write in one in eight, and in the rest leave each thread the program
  creates two to four slots, the others held elsewhere (section 8).
- Seizing a thread that finished exiting fails with EPERM as K-EXIT-5
  says, and needs no plan: a third of the seeds start the program
  untraced and let it run 0 to 999 instructions before the client acts,
  so attaching often meets threads in every stage of their exits.
- The EIO from `PTRACE_INTERRUPT` is not injected. The race behind it
  cannot be probed, and a fault the model cannot justify by a probe would
  break the rule that every behavior is pinned to Linux. The controller
  handles it, and real-kernel stress runs keep covering it.

**Preemption points.** With a per-seed chance (0, 2, 20, or 60 percent),
each call the controller makes into the kernel is preceded by one to
three of: a running thread executing up to 8 instructions, or the waiter
reaping a status. Their lines appear in the trace among the controller's
own, marked `preempt:`.

**Scheduling.** Half the seeds use the random walk of P1; the other half
use PCT with 0 to 3 change points among the first 64, 512, or 4,096
actions. The waiter, the controller, the client, and each thread are
actors with priorities. A thread that calls `sched_yield` drops below
every other actor, or a thread spinning on a barrier would starve the one
it waits for.

**Coverage marks.** `sim_mark!("fork child released after parent exit")`
counts how often a run reaches an interesting state. The gate requires
every mark to be reached at least once across its fixed seeds. That proves
the sweep reaches what it claims to test. Marks are added alongside the
faults and features they cover. P2 has 26 (`sim/marks.rs`), among them a
thread created, a leader exiting alone, a group exit taking a thread out
of its stop, two threads stopped at breakpoints in one stop, a thread run
or a status reaped inside a controller call, and each kind of SIGKILL. P3
adds six: a whole backtrace, one stopped at a frame without CFI, one ended
at an overwritten return address, a step judged, a source step judged
exactly, and a marker's condition holding. P4 adds 25, 57 in all, among
them a hit declined by its condition, a message logged, a watch hit
reported by a thread other than the first, a same-value store, a new
thread its busy slots left unarmed and the debugger refusing to run
because of it, a fork from a worker thread, a child released after its
parent exited, a SIGKILL right after a fork, a thread that could not be
seized, an interrupt reaching a stopped thread, and a released program
running on to its own end.

## 14. Running the simulator

| Command | What it does |
|---|---|
| `just` | The gate: the golden build and its checks; the kernel and CPU conformance tests; 1,000 fixed seeds over every program and variant (about 1.6 s); a determinism double-run of the first 32 seeds; the coverage-mark check; and ten sabotage tests showing the oracles catch lost trap writes, a deaf waiter, a thread resumed behind the controller's back, a trap the CPU skips, misreported return addresses, single steps that run on, misreported stack values (twice: variables and breakpoint conditions), debug-register writes that reach only a copy, and watch traps that never come. |
| `just sim [SECONDS]` | A sweep: random seeds on every core for SECONDS (default 30), inside `scripts/contained.sh`. Failures are grouped by their signatures (section 15); each group keeps its shortest run's report. |
| `just sim-seed SEED` | Replays one seed and prints its whole trace, also written to `target/sim/SEED/trace.log`. `--fingerprint` checks the replay against a report's fingerprint. |
| `just sim-seed SEED --at STEP` | Replays to STEP and prints the state there: each thread's state, report, pending signals, and `rip`; the waiter; the controller's queue; and the client. |

- **Profiles.** The gate uses the test profile. Sweeps use `[profile.sim]`:
  release optimizations with debug assertions, so the flight recorder and
  internal checks stay on.
- **Parallelism.** A sweep runs one world per worker thread. Worlds share
  only the immutable corpus: each binary's bytes, image, and `DebugInfo`,
  loaded once.
- **Memory safety.** Sweeps run under `scripts/contained.sh`: a
  `systemd-run --user` scope capped at half the memory, without swap, and
  first in line for the OOM killer, so a runaway session cannot take the
  machine's memory.
- **Throughput**, measured in P3 on a 12-core Ryzen AI 9 HX 370 (4 Zen 5
  and 8 Zen 5c cores) over all four programs and ten variants: about 1,100
  sessions per second on one thread (1,400 in P2, before the semantic
  oracles), about 9,000 per second in all on 12 threads, and about 8,600
  on 24. A one-minute sweep runs half a million sessions. In P4, with six
  programs, attach, watches, and conditions, a sweep on 24 threads runs
  about 6,300 sessions per second: 380,000 a minute.
- **Where sweeps run.** There is no CI today. Sweeps run locally, in the
  routine below.

**The routine**, as built in P5:

1. Before every commit, `just all` lints, runs the suite and `just
   stress`, and sweeps for 30 seconds, about 180,000 sessions.
2. Before merging a lifecycle, run-control, attach, or concurrency change,
   or a change to the model or the oracles, sweep for ten minutes: `just
   sim 600`, about 3.5 million sessions.
3. A failure's kind decides the response (section 15). Replay the shortest
   run of its group with `just sim-seed SEED --fingerprint F`, which
   checks that the run is the one the sweep saw, and print the state at an
   action with `--at STEP`. The trace includes the controller's flight
   recording.
4. A debugger failure gets a test outside the simulator, written to fail
   first, before the fix. Afterwards the seed replays cleanly, and a sweep
   of the same length finds nothing more.
5. An oracle found wrong is corrected in a change of its own that says
   why, with a unit test; it is never loosened to make a run pass.

The rules for working on the simulator itself are in AGENTS.md.

## 15. Failures: reports, replay, and grouping

**Three kinds of failure**, reported differently because they mean
different things:

| Kind | Meaning | Response |
|---|---|---|
| Debugger | An oracle disagreed with the debugger. | Write a red-first unit or scenario test that reproduces it outside the simulator, then fix. |
| Model gap | The program or the controller used something the simulator does not model: an instruction, syscall, ptrace request, or errno. | Probe the real behavior, add a rule with its conformance test, then model it. |
| Simulator | The simulator panicked or broke its own invariant. | Fix the simulator. |

**Report.** A failure prints everything needed to understand it without
rerunning:

```
SIM FAILURE  debugger  seed=0x5e1f00d2c3a4b8e1  commit=bf77646
program  threads-clang-O2-nofp  args=[4]
swarm    pct(d=2) queue=2 events=16 pipeline=3 attach=no
faults   sigkill@near-clone(2nd) [fired at #1832]
oracle   all-stop: stop 7 published while thread 1003 was running (since #1829)

  #1826  run 1003 x3 -> clone 1004
  #1827  deliver Wait(1002 Stopped SIGSTOP)
           PTRACE_GETSIGINFO 1002 -> SI_TKILL from tracer
  #1828  collect 1004 PtraceEvent(STOP)
  #1829  fault sigkill process 1001
  ...
  #1833  deliver Wait(1003 PtraceEvent(EXIT))
           publish Stop(7, Pause)

replay   just sim-seed 0x5e1f00d2c3a4b8e1
files    target/sim/0x5e1f00d2c3a4b8e1/{trace,flight,state}.log
```

**Replay** reruns the seed, verifies the fingerprint matches the failing
run's, and stops at the failing step.

**Seeds go stale.** A seed names a run only for the commit it ran on. Every
bug a seed finds becomes a red-first test in the ordinary suites before it
is fixed, as AGENTS.md requires. Seeds themselves are never kept as
regression tests.

**Grouping.** A sweep groups failures by their signatures: the kind, the
check, and the message with its numbers, and any dump of a value from the
first one on, left out (`Failure::signature`). Runs that meet one bug
differ in thread ids, addresses, and counts, not in the words around them,
so they share a group; two bugs that trip one oracle usually differ in
those words, and get a group each. Each group reports its count and its
shortest run, the fewest actions, with the smallest seed breaking ties.

**No shrinking.** The design once planned a tape mode, storing a run's
draws so that a shrinker could delete and zero stretches of them while the
run still failed. P5 decided against it, on the evidence of P1 to P4:

- None of the 27 debugger bugs and one oracle error those phases found
  needed it. Each report names the oracle, the action, and both sides of
  the disagreement; the cause was always within a few dozen lines of the
  trace's end; and `--at STEP` shows the whole state anywhere. The work
  was deciding whether the debugger, the oracle, or the model was wrong,
  which a shorter trace does not help with.
- It costs a rule every later change must keep: each random choice must
  make zero its simplest option. Breaking that rule fails silently, the
  shrinker merely shrinking worse. It adds a second way to replay a run,
  which needs its own determinism checks, and a definition of "the same
  failure" more exact than a signature.
- Most of its benefit comes free: a sweep meets a common failure many
  times, and keeps the shortest. To shorten one further, sweep again
  pinned to its program, variant, and faults.

Should failures someday become hard to read, there will be real examples
to design a shrinker around.

## 16. Production changes (P0)

P0 is the changes to the debugger itself that the simulator needs, made
before any simulator code exists so they can be reviewed apart from it. None
changes behavior. A change whose only user is simulator code cannot land
before that code: the crate denies unused items. Such changes are made in
P1, in the commit that first uses them.

Done:

1. **Hash iteration is linted** (section 6). The lints found two loops in
   DWARF loading whose order cannot change the result: one now uses a
   `BTreeMap`, the other says why order does not matter. They also found
   real nondeterminism at the edge: the DAP adapter reported breakpoint
   changes in hash order. Its groups are now a `BTreeMap`.
2. **Stop IDs come from the trace edge.** `LinuxTraceOps::allocate_stop_id`
   draws from the process-wide counter for live sessions and the test
   fakes. `SimTrace` will count per session. The post-mortem controller,
   which has no trace edge and is never simulated, calls the counter
   directly.
3. **`flight_recorder::Capture`.** While a capture lives, its thread's
   records go to a buffer of its own, without times or thread names;
   `take()` returns the lines since the last call. Other threads, and
   production, record as before. A world holds one for its whole run and
   takes the lines after each action, for its trace.

Found unnecessary:

- **Moving the session lease out of `Controller`.**
  `SessionLease::detached()`, which tests and post-mortem sessions already
  use, holds no global lease. A simulated controller takes one.
- **`DebugInfo: Clone`.** Its fields are public `Arc`s; the simulator
  builds each session's `DebugInfo` from the shared parts.
- **A `DebuggerHandle` constructor.** `DebuggerHandle` is defined in the
  crate root, so its private fields are visible everywhere in the crate.
  The simulator builds one around its own channels.

Moved to P1, with their first user:

- **`backend/linux/sim_edge.rs`**, the facade: build a
  `Controller<SimTrace>`, `handle_message`, and ground-truth queries for
  oracles (the sites the controller owns, the plan sites, the public stop).
- **`Waiter::external()`**, test-only today, is compiled for the `sim`
  feature too: `SimTrace::spawn_waiter` returns it.

Facts P1 must respect, found while preparing P0:

- `LinuxTraceOps` has default methods meant for test fakes:
  `thread_name` answers `None`, `module_mappings` an empty list, and
  `queued_trap` `false`. `SimTrace` overrides every one, so no answer is a
  plausible default.
- `DebuggerHandle`'s request futures need no runtime. Tokio's channels,
  `oneshot` replies, `broadcast` events with `Lagged`, backpressure, and
  `select! { biased; ... }` all work when polled by hand with a no-op
  waker, which was checked by experiment. `tokio::time` and `tokio::fs` do
  not: `timeout` panics outside a runtime. The simulated client therefore
  never calls `Debugger::shutdown` (it sends `Request::Shutdown` itself)
  or the source-context requests. An unbiased `select!` would draw from
  tokio's own random generator; every one in the request path is biased.
- Thread-local-storage lookups (`thread_db`, `glibc_tls`) read `/proc` and
  module files directly, outside `LinuxTraceOps`. The corpus has no TLS,
  so the simulator never reaches them. A session that did would read the
  host, so P1 checks it as a model gap.

## 17. Phases

Each phase ends with a short demo: a sweep, one failure replayed with
`just sim-seed`, and a walk through its report. The design stays
understandable as it grows, and each phase is reviewed before the next
begins.

**P0: Seams.** Done on 2026-10-04; section 16 lists what changed. *Exit:*
the gate passes; there are no behavior changes.

**P1: One thread, end to end.** Done on 2026-10-04.

- The golden runtime and `straight` in four variants (GCC and Clang, `-O0`
  and `-O2`, static), with `just golden-build` and `golden-check`. Builds
  are reproducible and name sources under `/uscope`, wherever the
  repository is checked out.
- The single-threaded kernel: launch, `int3`, single steps, SIGSTOP from
  the tracer, faults, `write`, `exit_group`, SIGKILL, and the exit event,
  each rule probed by a dual-run test.
- The interpreter for every instruction the corpus executes, checked by
  lockstep over every variant and argument list (about 27,000
  instructions, 0.2 s).
- The world, the swarm, the client, the oracles (protocol, code integrity,
  site ownership, events, transparency, clean exit, liveness), coverage
  marks, reports, replay, and the gate.

Departures from the design, each for a reason recorded where it applies:
the simulator compiles for tests without a feature flag (section 5);
`SimTrace` lives in the facade (section 5); and K-EXEC-1 was added
(section 8).

The first sweeps found two debugger bugs. Each now has a red-first test
outside the simulator, as section 15 requires:

- A pause that arrived once every thread was past its exit event failed
  with "the inferior is not stopped". It is now accepted and ends with the
  exit (`a_pause_after_every_thread_began_exiting_ends_with_the_exit`).
- An instruction step from a stop whose inline frame is ambiguous was
  refused, though it needs no frame
  (`instruction_steps_work_where_the_inline_frame_is_ambiguous`).

The sweeps also showed where the client's expectations were wrong, and
those were corrected: events share the revision of the state change that
produced them; explicit refusals are correct for lines without code,
functions another variant inlined away, a step out of the outermost
frame, and source steps and backtraces from an ambiguous inline frame;
and a kill or pause sent while the program runs may meet its exit.

*Exit:* met. `straight` in all its variants is deterministic under the
double-run check, every mark is reached by the fixed seeds, and 650,000
swept sessions found nothing more.

**P2: Threads and kills.** Done on 2026-10-04.

- `clone`, `exit`, and `exit_group`, with the kernel modeling `orig_rax`,
  system calls a thread stops inside, siginfo per pending signal, a
  leader's delayed exit, and group exits (section 8).
- Pause and stop barriers, through the controller's `tgkill` requests;
  preemption points inside every controller call (section 13).
- PCT scheduling beside the random walk (section 13).
- External SIGKILL faults with plans, firing counts, and coverage marks;
  programs exit their own groups (section 13).
- Dual-run kernel conformance tests for every rule used: 15 probes over
  every variant. The lockstep test follows threads (section 9 and
  `sim/conformance/cpu.rs`) and checks `xadd` and `adc` too.
- The all-stop, breakpoint-accounting, and thread-exit oracles
  (section 12), each with a unit test, and a sabotage test for each of the
  first two.
- The `threads` and `racing-exit` programs, on the thread runtime
  (section 11).

Departures from the design, each for a reason recorded where it applies:
`PTRACE_INTERRUPT` moves to P4 with attach, the only sessions that use it
(section 8); thread stacks are static, so `mmap` is not modeled; and the
`exit_group` fault is the programs' own (section 13).

The sweeps found four debugger bugs. Each now has a red-first test
outside the simulator, as section 15 requires:

- A pause never completed once the main thread had exited alone, since
  Linux reports its exit only after every other thread's
  (`pause_stops_a_process_whose_main_thread_exited`, a scenario). A
  breakpoint added at that stop was never written, and detaching an
  attached process in that state never finished
  (`detaching_waits_for_no_main_thread_that_exited_after_attaching`).
  Such a leader now settles every stop, and stops no longer list it, as
  attaching already did not.
- A pause that met such a leader's exit event never re-checked its
  barrier, and then tried to present the stop through the zombie leader
  (`a_pause_completes_when_the_main_thread_exits_alone`).
- A pause failed with "not stopped" when no thread was left to ask: the
  others held after a thread resumed alone exited, or still to report
  their first stop, or one past its exit event
  (`a_pause_after_a_lone_main_thread_exited_stops_at_once`,
  `a_pause_waits_for_a_starting_thread_to_stop`,
  `a_pause_waits_for_an_exiting_thread`).
- A step whose plan ended while siblings ran removed its sites although a
  sibling had already executed one of their traps. That trap was then
  published as unclassifiable, with the thread's `rip` one byte into an
  instruction. A trap one byte past a removed site now rewinds the thread,
  which runs on (`a_trap_reported_after_its_site_was_removed_runs_on`).

Two probes corrected the model before any session relied on it (section
8). The sweeps also corrected the client: a thread resumed alone may wait
forever for a sibling it holds, so the client pauses such an execution
rather than waiting; and a request about a process ending as a whole may
fail however it fails, since the debugger may have published a stop just
before hearing of the end.

*Exit:* met. Every rule in use has a passing probe, all 26 marks are
reached by the fixed seeds, and 2.7 million swept sessions found nothing
more.

**P3: Compiler breadth and semantic oracles.** Done on 2026-10-04.

- Ten variants of every program: GCC and Clang, at `-O0` and `-O2`, with
  and without frame pointers, and two static-PIE builds whose runtime
  relocates itself (section 11). The loader places static-PIE images as
  Linux does, which K-EXEC-2 pins on every variant (section 8).
- The interpreter passed lockstep on all 40 binaries without a new
  instruction: the new flags produced nothing the corpus had not used.
- `facts.json` from GNU binutils, markers in the sources, the kernel's
  shadow state, and the backtrace, stepping, and variables oracles, each
  with a sabotage test and the two subtlest with unit tests (sections 10
  to 12). Six coverage marks prove the sweep reaches them.
- The `frames` program, with its hand-written frames (section 11).

Departures from the design, each for a reason recorded where it applies:
binutils stand in for `llvm-dwarfdump` (section 11); markers live in the
sources, not the manifest (section 11); and the hand-written assembly is
part of `frames.c` rather than programs of its own (section 11).

The sweeps found six debugger bugs. Each now has a red-first test outside
the simulator, as section 15 requires:

- A step over begun in code without call-frame information was refused,
  though uscope documents that it steps as stepping in does there
  (`stepping_over_from_code_without_unwind_information_stops_at_the_first_source_statement`).
  Stepping in from such code stopped at a function's opening line, before
  its prologue: without a starting frame, it now recognizes a function it
  entered by the stack pointer the step began at.
- A step out from a line Clang marks as ending in an epilogue ran one
  instruction past the return address, crossing to the caller's next line
  as a step over does
  (`step_out_across_a_marked_epilogue_stops_at_the_return_address`).
- A step over a function's last line returned into the middle of a caller
  that called the function again, and stopped inside the second call: the
  controller knows frames by their CFA, and the new frame had the old one's
  (`stepping_over_a_return_stops_in_the_caller_before_a_second_call`). Once
  its frame has returned, a step over now records the frame it returned
  to, judges by that, and guards callees' returns; when that frame returns
  too, it moves outward
  (`a_step_over_does_not_stop_in_a_new_frame_where_a_returned_one_was`).
- A step in from a callee's last line, after crossing its epilogue to a
  caller that returned before reaching any statement, ran on past the outer
  caller's lines, to the program's exit
  (`source_steps_return_through_a_caller_with_nothing_left_to_run`).
- Variables read at a function's first instruction, where Clang's
  unoptimized frame base is still the zero `_start` left in `rbp`, failed
  the whole request with an address overflow; that variable is now
  unavailable (`a_variable_whose_location_wraps_the_address_space_is_unavailable`).
- Hand-written assembly placed after a C function was presented as that
  function's last line, since GCC's last row runs on through it, and a step
  in could stop there. No line entry now runs past the start of a function
  (`hand_written_assembly_after_a_function_has_no_source_line`).

The sweeps also corrected the oracles and the client before trusting them:
rows at one address collapse as gdb collapses them; a source step never
stops at an epilogue marker; a step from inside a system call may only
finish the call; a step out goes on past a return address no line
describes; and stepping out of a frame whose caller is outside every
module, or reading the variables of undescribed code, is refused.

*Exit:* met. Every variant passes lockstep; the semantic oracles run at
every stop of every session, all 32 marks are reached by the fixed seeds,
and 6.5 million swept sessions found nothing more.

**P4: Fork, attach, and watchpoints.** Done on 2026-10-05.

- Fork children and their release: `fork` and `wait4`, children that
  outlive their parents, and the reaper that inherits them (K-FORK-1 to
  K-FORK-3, section 8).
- Attach after an untraced run, in a third of the seeds, with
  `PTRACE_SEIZE`, `PTRACE_INTERRUPT`, and the tracer's exit probed and
  modeled (K-SEIZE-1, K-INT-1, K-INT-2, K-WAIT-3).
- Debug registers in their three behaviors (K-DR-1 to K-DR-5), and the
  watch-accounting oracle (sections 8 and 12).
- Hit conditions, conditions, and log messages in the client, set when a
  breakpoint is added and changed while the program runs or is stopped,
  with the breakpoint-conditions oracle; hit counts judged per arrival
  (section 12).
- The `fork` and `stores` programs (section 11).
- 15 new dual-run probes, 31 in all, so every rule in section 8 has one.
  The gate runs 1,000 fixed seeds, up from 300, so that every one of the
  57 marks is reached.
- The client split along the seams section 5 named:
  `sim/client/breakpoints.rs` and `sim/client/stops.rs`.
- Golden binaries and facts are built into `build/golden` and verified
  against the manifests, no longer checked in (sections 11 and 18).

Departures from the design, each for a reason recorded where it applies:
the EIO an interrupt can meet is not modeled, since no probe can produce it
(section 13); a tracer that exits holding a thread in a ptrace-stop other
than its exit event, or with `PTRACE_O_EXITKILL` and live threads, is a
model gap, and an oracle keeps the controller from the first (sections 8
and 12); seizing an exiting thread needs no planned fault (section 13);
and P2's hit-count rule was wrong where a thread executes a trap twice in
one arrival (section 12).

The sweeps found fifteen debugger bugs. Each now has a red-first test
outside the simulator, as section 15 requires.

*Hit counts and conditions:*

- A thread that hit a breakpoint, when the breakpoint was replaced by
  another at the same place before it ran on, hit the new one at once,
  counting its arrival twice. A thread now steps over the trap it
  reported, whichever breakpoint owns it by then, even when a pause
  interrupted its step over it; a thread a step left at a new breakpoint
  hits it as it resumes
  (`only_a_thread_that_hit_a_breakpoint_steps_over_one_added_where_it_stands`,
  `a_thread_kept_stopped_steps_over_a_breakpoint_replaced_where_it_hit_one`,
  `a_breakpoint_replaced_where_a_paused_repair_stands_is_stepped_over`).
- A thread stepping from a breakpoint it had not hit met a signal first.
  Back from the signal, it hit the breakpoint at the guard where its step
  resumed, and the step then executed the trap again rather than the
  instruction it covers
  (`a_breakpoint_hit_where_a_signal_interrupted_a_step_is_stepped_over`).
- A next over a call ended early at a hit its condition declined two calls
  down: at the line after that hit's caller
  (`skipped_hits_deep_inside_a_call_are_transparent_to_next`), or, at a
  hit just before a callee's marked epilogue, as if that were the stepping
  frame's own
  (`a_skipped_hit_before_a_deep_callees_epilogue_is_transparent_to_next`).
- A step out to code without debug information ended at a later hit its
  breakpoint's hit condition declined
  (`a_step_out_to_undescribed_code_does_not_end_at_a_declined_hit`), and
  one to a return address no line describes ran on past the caller
  instead of stopping at its first described instruction
  (`a_step_out_to_undescribed_code_in_the_caller_stops_in_the_caller`).
- A breakpoint removed while SIGKILL took a sibling out of a published
  stop left its owner on the trap until the sibling was reaped, and
  presenting a stopped thread's frames panicked on it
  (`frames_ignore_a_breakpoint_removed_while_a_sibling_exits`).
- A whole C array has no byte size in its DWARF, so it was not watched by
  its elements' size; its size is now its elements', laid end to end
  unless a stride spaces them
  (`whole_arrays_are_watched_by_their_elements_size`).

*Fork children:*

- A shutdown meeting a fork event that the parent's SIGKILL superseded
  left the child stopped and traced
  (`a_shutdown_meeting_a_superseded_fork_event_still_releases_the_child`).
- A launch while the last program's fork children were still being
  released failed as if a program still ran
  (`a_launch_while_fork_children_are_released_waits_for_them`).

*Attach and detach:*

- Detaching an attached process that SIGKILL from outside had taken out
  of its stop failed, though its traps can never run again
  (`detaching_a_process_killed_from_outside_succeeds`).
- A main thread that reached its exit event as an attached session ended
  kept the detach from finishing: Linux reports its exit only after every
  other thread's
  (`detaching_waits_for_no_main_thread_that_exits_during_the_detach`).
- Removing a watchpoint while a thread had lost its slots to another user
  failed to arm that thread, and the rollback killed the attached process.
  Such a thread is now left to be armed before it runs
  (`editing_watchpoints_leaves_a_thread_that_lost_its_slots_to_the_resume`).
- A failure that detached an attached process left the controller serving
  requests, rather than ending as a shutdown does
  (`a_failure_that_detaches_an_attached_process_ends_the_controller`).

*Threads exiting under run control:*

- A step whose main thread exited alone, while its siblings ran on in the
  step's scope, tried to step the main thread again once a sibling's
  repair finished
  (`a_step_whose_main_thread_exited_alone_runs_its_siblings_on`).
- A thread waiting to step over its breakpoint while SIGKILL took it to
  its exit was still stepped over it; its repair is now skipped
  (`an_exiting_thread_is_not_stepped_over_its_breakpoint`).

The sweeps also corrected the model and the client before trusting them:
a released program's later requests may fail however they fail; the
client may meet an attached program that ended before it asked; a
debugger whose attached program fails under it detaches and exits; the
oracles judge only the process the controller has finished launching or
attaching to; and a trap heard during a shutdown is no hit, which only the
final 7.5-million-session sweep met, attached, with a worker trapped just
as the session ended (section 12).

*Exit:* met. Every rule has a passing probe, all 57 marks are reached by
the fixed seeds, the gate and `just stress` pass, and of 10.9 million
swept sessions the first 7.5 million found the one oracle error above and
the 3.4 million after its correction found nothing.

**P5: Grouping and routine.** Done on 2026-10-05.

- A sweep groups failures by their signatures rather than by oracle
  alone, so two bugs that trip one oracle are not merged, and each group
  keeps its shortest run rather than its smallest seed (section 15).
- `just all` runs lint, the suite, `just stress`, and a 30-second sweep
  before every commit, and the routine for longer sweeps is written down
  (section 14).
- AGENTS.md has a section of its own for the simulator: the determinism
  contract, rules need probes, never loosen an oracle, what each kind of
  failure calls for, the routine, and the golden programs.

Departure from the design: tape mode and the shrinker were dropped, for
the reasons section 15 records.

## 18. Open questions

Decided on 2026-10-04: the simulator is in-crate, behind the `sim` feature
(section 5), and DAP and the CLI stay outside it (section 2).

1. **Corpus storage.** Decided on 2026-10-05: binaries are built, not
   checked in. The built corpus reached 1.8 MB in P4, against a 2 MB budget
   for plain git, and every rebuild would have added to history for good.
   The pinned toolchain reproduces every binary byte for byte, so the
   manifests' hashes pin them as firmly as checked-in files did. Git LFS
   was rejected for its tooling and quotas, and fewer variants for the
   coverage they give. Binaries already in history stay there; nothing
   new is added.
2. **Ambiguous stops from nested inline breakpoints.** When breakpoints on
   two nested inlined functions hit at the same address, as `rt_exit_group`
   and `rt_syscall3` do in `straight-clang-O2` at `0x401334`, the stop
   presents its inline frame as ambiguous, so source steps and backtraces
   there are refused. The hits are consistent: one inline chain holds both.
   Presenting the innermost hit's frame would keep those requests working.
   Is the ambiguity intended? The simulator's client accepts the refusals
   until this is decided.
3. **An execution whose only thread exited alone as the leader.** When the
   client resumes the main thread alone and it exits while others live,
   Linux reports its exit only after theirs, and they are held stopped, so
   the execution can never end by itself; a pause stops it at once. gdb
   reports a thread exit and stops instead. Should uscope end such an
   execution with `ThreadExited` when the leader passes its exit event? Its
   final status is not known then (K-EXIT-6).

## Glossary

- **Action:** one atomic thing the world does in a step (section 4).
- **Fingerprint:** the hash of a run's trace. Equal fingerprints mean
  identical runs.
- **Model gap:** something real uscope or a program does that the
  simulator does not model. It is reported as such, never guessed.
- **Oracle:** a check that compares the debugger's behavior with the
  simulator's ground truth.
- **PCT:** probabilistic concurrency testing (Burckhardt et al., 2010).
  Threads get random priorities that change at a few random points, which
  finds ordering bugs of small depth with known probability.
- **Preemption point:** a `SimTrace` call at which running threads may
  advance before the call takes effect.
- **Signature:** what failures with one cause share, by which a sweep
  groups them (section 15).
- **Swarm configuration:** the per-seed choice of which features and faults
  are active, and how intensely.
