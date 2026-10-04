# Deterministic Simulation

Status: design, 2026-10-04. Nothing here is implemented yet.

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

The simulator lives in the uscope crate, under `src/sim/`, compiled only
with the `sim` cargo feature. Release builds never contain it. Being in the
crate lets it use the controller's private types through one narrow facade
without widening the public API. The cost is that `just` builds the crate
with `--features sim`, adding the simulator to every test build; P1 measures
that cost against the fast-iteration budget.

| Module | Responsibility | Size goal |
|---|---|---|
| `sim/choices.rs` | Seed expansion, PRNG, streams, swarm configuration | 300 |
| `sim/world.rs` | The step loop, action selection, the controller queue, the waiter actor | 500 |
| `sim/schedule.rs` | Random-walk and PCT scheduling policies | 200 |
| `sim/kernel/mod.rs` | Process and thread tables, wait queue, reaping | 500 |
| `sim/kernel/ptrace.rs` | Per-thread ptrace state machine and request semantics | 600 |
| `sim/kernel/signals.rs` | Signal generation, delivery stops, SIGKILL and exit zapping | 400 |
| `sim/kernel/syscalls.rs` | The syscalls the corpus runtime makes | 300 |
| `sim/kernel/debug_regs.rs` | DR0–DR7 per thread, hit detection, DR6 | 300 |
| `sim/cpu/mod.rs` | Instruction dispatch and outcomes | 400 |
| `sim/cpu/ops/*.rs` | Instruction semantics grouped by family | 1,500 total |
| `sim/memory.rs` | Copy-on-write address spaces and page protections | 250 |
| `sim/loader.rs` | Loads golden ELF images into an address space | 200 |
| `sim/trace.rs` | `SimTrace`, the `LinuxTraceOps` implementation | 400 |
| `sim/client.rs` | Client tasks that drive `DebuggerHandle` | 600 |
| `sim/oracles/*.rs` | One file per oracle family (section 12) | 1,000 total |
| `sim/faults.rs` | The fault catalog and fault plans | 250 |
| `sim/report.rs` | Trace lines, fingerprints, failure reports | 300 |
| `sim/marks.rs` | Coverage marks and their counts | 100 |
| `src/bin/uscope-sim.rs` | Sweep, replay, and state-dump commands | 300 |
| `backend/linux/sim_edge.rs` | The facade: build a controller, deliver a message, read ground truth for oracles | 150 |

Size goals are reading budgets, not hard limits. A module that grows well
past its goal is split along a seam a reader would recognize.

## 6. Determinism contract

A run is a pure function of its seed and the code. These rules keep it so,
and each one is enforced rather than hoped for.

| Hazard | Rule | Enforcement |
|---|---|---|
| Random choices | Every choice comes from `Choices`. Nothing else in the run calls a PRNG, `getrandom`, or `RandomState`. | Review; the fingerprint check catches leaks. |
| Hash iteration order | Never iterate a `HashMap` or `HashSet` where order can affect behavior. Lookups are fine. | `clippy::iter_over_hash_type` denied crate-wide. |
| Time | Simulated code reads no clock. The flight recorder's capture sink omits timestamps. Waiter backoff and client timeouts never run in a session. | The sim's facade constructs no waiter thread and no tokio timer. |
| Process-wide state | The session lease, `NEXT_STOP_ID`, and the flight recorder's ring are process-global today. Sessions on parallel worker threads must not share them. | P0 moves each behind an injected per-session source (section 16). |
| Threads | A run uses exactly one OS thread. | Nothing in the sim spawns threads. The controller under simulation never calls `spawn_waiter`'s thread path. |
| Host files and `/proc` | Simulated sessions read no host state except the golden corpus, loaded once and shared read-only. | All controller host access already goes through `LinuxTraceOps` (`5e939d9`). |
| Pointer identity | Never order or key data by memory address. | Review. |

**Fingerprint.** Every action appends a line to the run's trace. That
includes each `SimTrace` call with its result, through the existing
`Recorded` wrapper. The fingerprint is a 64-bit FNV-1a hash of the trace.
The gate replays a sample of seeds twice and compares fingerprints, so a
determinism leak fails `just` rather than surfacing as an unreproducible
sweep failure.

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
  and the value chosen. A later phase stores draws as a tape, which makes
  shrinking possible (section 15).

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
| K-WAIT-1 | A thread has at most one reportable status. A thread woken out of a stop loses an unreported one. Ptrace requests on a thread not in a ptrace-stop fail with ESRCH. |
| K-WAIT-2 | Which ready status a wait returns is unspecified, so the scheduler chooses. One exception: a group leader's exit is reported after every other thread's. |
| K-TRAP-1 | `int3` raises SIGTRAP with `si_code` `SI_KERNEL` and `rip` after the trap byte. A single step reports `TRAP_TRACE`. A step across `syscall` reports `TRAP_BRKPT`. |
| K-SIG-1 | The tracer's `tgkill(SIGSTOP)` produces a signal-delivery stop with `si_code` `SI_TKILL` and the tracer's process as sender. |
| K-INT-1 | `PTRACE_INTERRUPT` stops a running thread at its next kernel entry with `PTRACE_EVENT_STOP`. One sent to a thread that is already stopped stays pending until the thread resumes, and the thread's next stop of any kind consumes it. |
| K-INT-2 | `PTRACE_INTERRUPT` and `tgkill` return 0 for a thread at its exit event or an unreaped zombie, and ESRCH once it is reaped. When the reap lands inside the interrupt's own window, the interrupt fails with EIO (a fault, section 13). |
| K-EXIT-1 | `exit_group` with running siblings: every thread stops at `PTRACE_EVENT_EXIT`, with message `code << 8` and `si_code` `0x605`. After `PTRACE_CONT`, each reports its exit, the leader's last. |
| K-EXIT-2 | Siblings held in signal-delivery stops are pulled out of them and stop at `PTRACE_EVENT_EXIT` too. |
| K-EXIT-3 | SIGKILL from anywhere: every thread stops at `PTRACE_EVENT_EXIT` with message 9, then is reported killed by signal 9. |
| K-EXIT-4 | A thread already at its exit stop is not released by SIGKILL. It answers `GETREGS` and memory reads, and waits for `PTRACE_CONT`. |
| K-EXIT-5 | A leader that exits alone stays a zombie until the last thread exits. Its `exe` link is gone, its maps read empty, and ptrace requests on it fail. Seizing a zombie fails with EPERM. |
| K-CLONE-1 | Whether a clone's child is traced is decided at the syscall, from the parent's options at that moment. The child's first stop and the parent's clone event become reportable in either order. |
| K-FORK-1 | A fork child gets a copy of the parent's address space, traps included. It is auto-attached with the parent's options and starts in a stop. |
| K-DR-1 | A watch hit raises SIGTRAP with `TRAP_HWBKPT` and `rip` after the instruction. DR6 changes only at debug exceptions and is stale at every other stop. |
| K-DR-2 | Single-stepping over a watched store gives one stop: `TRAP_TRACE`, with DR6 holding both the single-step bit and the watch bit. |
| K-DR-3 | New threads and fork children start with debug registers disarmed, but `PEEKUSER` of DR7 returns the creator's value. Detaching does not clear them. |
| K-DR-4 | Writing a debug-register address reserves a slot even while disabled, and can fail with ENOSPC. DR7 writes are transactional. |
| K-DR-5 | `rep stos` and `rep movs` trap once per iteration that touches the watched range, with `rip` still at the instruction. Stores of the same value trap. `POKEDATA` never traps. |
| K-MEM-1 | `PEEKDATA` and `POKEDATA` ignore page protections and fail only where nothing is mapped. CPU accesses obey protections and fault with `SEGV_MAPERR` or `SEGV_ACCERR`. |

**Request semantics.** `sim/kernel/ptrace.rs` has one function per
`LinuxTraceOps` method. Each documents its errno outcomes in terms of the
rules above. For example, `continue_execution` on a thread that has left
its stop returns ESRCH (K-WAIT-1). Methods the controller uses but the
simulator does not support yet return a model-gap failure, never a
plausible default.

**Syscalls** modeled for the corpus runtime: `write` (captured as the
program's output), `exit`, `exit_group`, `clone` (threads and fork),
`wait4` (for forking programs), `mmap` and `munmap` (anonymous only),
`sched_yield`, `getpid`, `gettid`, `tgkill`, and `nanosleep` (yields
without consuming time). Any other syscall is a model gap.

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

**Memory.** An address space is a map of 4 KiB pages with protections.
Pages loaded from a golden image are shared, and copied on first write, so
a session costs little more than its stack and the pages its program
writes. Fork copies the page map, not the pages. CPU accesses obey
protections; ptrace accesses do not (K-MEM-1).

**Loader.** `sim/loader.rs` maps the `PT_LOAD` segments of a static
executable, builds the initial stack (`argc`, `argv`, an empty environment,
and a minimal auxiliary vector: `AT_PAGESZ`, `AT_ENTRY`, `AT_PHDR`,
`AT_RANDOM` from the seed), and sets `rip` to the entry point. ASLR is off,
as uscope launches programs. Static-PIE images load at the address Linux
uses for an unrandomized `ET_DYN` executable, and their runtime relocates
itself.

## 11. The golden corpus

Programs live in `tests/golden/`, checked in as source and compiled
binaries, so compiler upgrades never change what a test means.

```
tests/golden/
  rt/                     the freestanding runtime: _start, syscalls, threads
  straight/
    straight.c            the source, with marker comments (// MARK: name)
    manifest.json         variants, hashes, expected behavior
    facts.json            line table and symbols from llvm-dwarfdump and nm
    straight-gcc-O0       one binary per variant
    straight-clang-O2-nofp
    ...
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

- `just golden-build NAME` compiles one program's variants with the
  Nix-pinned toolchain. It regenerates `facts.json` and records every hash
  in the manifest.
- `just golden-check` runs in the gate. It fails when a binary's hash, or
  its source's hash, differs from the manifest. A source edited without a
  rebuild, or a binary changed by hand, cannot go unnoticed.
- Binaries are rebuilt only on purpose, in a commit of their own.

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

**Size budget.** Each binary is a few kilobytes plus its DWARF. The whole
corpus stays under 2 MB in plain git.

**Initial programs.**

| Program | Exercises |
|---|---|
| `straight` | Loops, calls, recursion, and inlining in one thread. |
| `threads` | Workers created with raw `clone`, barriers, shared counters, and workers ending by `exit` and by `exit_group`. |
| `racing-exit` | `exit_group` while siblings run and hit breakpoints. |
| `fork` | A fork child that outlives its parent, a child reaped by the parent, and forks racing breakpoint edits. |
| `stores` | Global stores, same-value stores, `rep stos`, and adjacent watched ranges. |
| `frames` | Tail calls, `-O2` frames without frame pointers, and the hand-written orphan and CFI-less frames. |

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

## 13. Faults

Faults are things the real world can do to a debugging session. Each one
carries the kernel rule or real behavior that justifies it. The simulator
never injects a failure Linux cannot produce.

| Fault | Justification |
|---|---|
| External SIGKILL at a random step | Any process can be killed (K-EXIT-3). |
| SIGKILL near a clone, fork, or exec event, or between two trace calls | The same, aimed where v1 found most bugs. |
| A sibling's `exit_group` while a stop is handled | Programs exit whenever they like (K-EXIT-1, K-EXIT-2). |
| A thread reaped inside `PTRACE_INTERRUPT`'s window, returning EIO | K-INT-2, observed in stress runs. |
| Seizing a thread that is just exiting fails with EPERM | K-EXIT-5. |
| Debug-register writes discarded, or failing with ENOSPC | gVisor, and slots held by perf (K-DR-4). |
| Tiny queue and broadcast capacities | Legal configurations of the real channels. |
| The client pipelines requests and resends stale `StopId`s | Real clients race their own requests. |

**Plans.** The swarm turns each fault kind on or off per seed. Enabled
faults fire by plan: "the 3rd clone event", "50 steps after the first
stop". This aims faults at interesting moments far more often than uniform
chance would. Every planned fault must fire, or the run reports it as
unfired. A sweep where faults silently never happen is useless.

**Coverage marks.** `sim_mark!("fork child released after parent exit")`
counts how often a run reaches an interesting state. The gate requires
every mark to be reached at least once across its fixed seeds. That proves
the sweep reaches what it claims to test. Marks are added alongside the
faults and features they cover.

## 14. Running the simulator

| Command | What it does |
|---|---|
| `just` | The gate: `golden-check`; kernel and CPU conformance tests; fixed seeds 0..N over every program and variant (about 2 s of wall time); a determinism double-run of the first 32 seeds; and the coverage-mark check. |
| `just sim [SECONDS]` | A sweep: random seeds on every core for SECONDS (default 60), run inside `scripts/contained.sh`. Failures are grouped by oracle and site; each group keeps its smallest seed. |
| `just sim-seed SEED` | Replays one seed. Prints the swarm configuration, the full trace, and the flight recording, and writes them to `target/sim/SEED/`. |
| `just sim-seed SEED --at STEP` | Replays to STEP and prints the kernel's state there: each thread's registers, stop state, and pending status; the controller queue; and the outstanding requests. |

- **Profiles.** The gate uses the test profile. Sweeps use `[profile.sim]`:
  release optimizations with debug assertions, so the flight recorder and
  internal checks stay on.
- **Parallelism.** A sweep runs one world per worker thread. Worlds share
  only the immutable corpus: each binary's bytes and its `DebugInfo`, loaded
  once.
- **Memory safety.** Sweeps run under a new `scripts/contained.sh`. It runs
  the sweep in a `systemd-run --user` scope with a memory cap and a high OOM
  score, so a runaway session cannot exhaust the machine's memory.
- **Throughput targets**, to be measured and revised in P1:
  - 1,000 sessions per second per core for `straight`;
  - 300 per second per core for the threaded programs.
- **Where sweeps run.** There is no CI today. Sweeps run locally, before
  merging any lifecycle, run-control, or concurrency change, alongside
  `just stress`.

## 15. Failures: reports, replay, and shrinking

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

**Shrinking (P5).** In tape mode, a run's draws are stored as a sequence
of integers, and an exhausted tape draws zero. Zero always means the
simplest option: the fewest threads, no faults, the first enabled action.
A shrinker deletes and zeroes stretches of the tape. It keeps each change
that still fails the same oracle at the same site. The result is a short
trace a person can read in one sitting.

## 16. Production changes (P0)

These are small, independent commits that make production code simulable
without behavior change. Each is tested by the existing suite.

1. **Session lease out of `Controller`.** `spawn_controller` acquires the
   lease and holds it in the controller thread's closure. The lease guards
   real ptrace, which a simulated session never touches.
2. **Stop-ID source.** `Controller` takes its `StopId` allocator as a
   field. Production passes the process-wide atomic, which keeps IDs unique
   across sessions. The simulator passes a counter per session.
3. **Flight-recorder capture scope.** `flight_recorder::capture(|| ...)`
   sends the current thread's records to a buffer, without timestamps or
   thread names, for the duration of a closure. Production recording is
   unchanged.
4. **`DebugInfo: Clone`.** All its fields are `Arc`s already.
5. **`DebuggerHandle` from channels.** A crate-private constructor builds a
   handle on given channels, so the simulator gets the real API without
   `Debugger::new`'s thread and lease.
6. **`clippy::iter_over_hash_type`** denied crate-wide, fixing any
   order-dependent iteration it finds.
7. **`backend/linux/sim_edge.rs`** (feature `sim`), the facade:
   - build a `Controller<SimTrace>`;
   - `handle_message`;
   - ground-truth queries for oracles: the sites the controller owns, the
     plan sites, and the public stop.

## 17. Phases

Each phase ends with a short demo: a sweep, one failure replayed with
`just sim-seed`, and a walk through its report. The design stays
understandable as it grows, and each phase is reviewed before the next
begins.

**P0: Seams.** The production changes in section 16. *Exit:* the gate
passes; there are no behavior changes.

**P1: One thread, end to end.**

- `Choices`, the world loop, and the random-walk scheduler.
- The kernel for one single-threaded process: launch, traps, single step,
  exit, and SIGKILL.
- The interpreter for what `straight` needs, with the lockstep test.
- The loader, `SimTrace`, and a client that launches, sets and removes
  breakpoints, continues, steps, reads memory, backtraces, kills, and shuts
  down through `DebuggerHandle`.
- Oracles: protocol, code integrity, clean exit, transparency, and liveness.
- The runner, the report, `sim-seed`, the fingerprint, and the gate.
- `golden-build`, `golden-check`, and the runtime, for `straight`.

*Exit:* `straight` in all its variants is deterministic under the
double-run check, and the throughput target is measured.

**P2: Threads and kills.**

- `clone`, `exit`, and `exit_group`.
- Pause and stop barriers; `PTRACE_INTERRUPT`; preemption points.
- PCT scheduling.
- External SIGKILL and `exit_group` faults, with plans and coverage marks.
- Dual-run kernel conformance tests for every rule used.
- The all-stop and breakpoint-accounting oracles.
- The `threads` and `racing-exit` programs.

*Exit:* every K-* rule in use has a passing probe, and every mark is
reached.

**P3: Compiler breadth and semantic oracles.**

- GCC and Clang, at `-O0` and `-O2`, with and without frame pointers, and
  static-PIE.
- The interpreter grows under lockstep.
- `facts.json`; the stepping, backtrace, and variable oracles.
- The `frames` program and the hand-written assembly programs.

*Exit:* every variant passes lockstep, and the semantic oracles run in
every session.

**P4: Fork, attach, and watchpoints.**

- Fork children and their release.
- Attach after an untraced run.
- Debug registers in their three behaviors, and the watch-accounting
  oracle.
- Hit counts and conditions in the client.
- The `fork` and `stores` programs.

**P5: Shrinking and routine.**

- Tape mode and the shrinker.
- Grouping of failures in sweep summaries.
- A documented routine for pre-merge sweeps.
- AGENTS.md rules for working on the simulator, in a section of its own:
  - the determinism contract;
  - rules need probes;
  - never loosen an oracle;
  - model gaps are not debugger bugs.

## 18. Open questions

Decided on 2026-10-04: the simulator is in-crate, behind the `sim` feature
(section 5), and DAP and the CLI stay outside it (section 2).

1. **Corpus storage.** Plain git is assumed while the corpus stays under
   2 MB. Should a larger budget ever be needed, the choice is between Git
   LFS and building in Nix with pinned hashes.

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
- **Swarm configuration:** the per-seed choice of which features and faults
  are active, and how intensely.
- **Tape:** the recorded sequence of a run's random draws, which can be
  replayed or shrunk.
