# Deterministic Simulation

Status: P0, P1, and P2 done, 2026-10-04 (section 17). P3 onward is design.

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

As built in P2, with sizes in lines:

| Module | Responsibility | Lines |
|---|---|---|
| `sim/choices.rs` | Seed expansion, the PRNG, streams | 246 |
| `sim/swarm.rs` | The run's shape, chosen before it starts | 121 |
| `sim/schedule.rs` | The random walk and PCT | 222 |
| `sim/faults.rs` | Fault plans | 97 |
| `sim/world.rs` | The step loop, delivery, checks after each action | 635 |
| `sim/machine.rs` | What actions and preemption points reach: running threads, the waiter, faults | 230 |
| `sim/audit.rs` | The event auditor | 135 |
| `sim/kernel/mod.rs` | Process and thread tables, run states, reaping, traps | 683 |
| `sim/kernel/ptrace.rs` | Ptrace requests and their errnos | 163 |
| `sim/kernel/signals.rs` | Signal delivery, group exits, and the exit paths | 281 |
| `sim/kernel/syscalls.rs` | `write`, `sched_yield`, `clone`, `exit`, and `exit_group` | 169 |
| `sim/cpu/mod.rs` | Registers, decoding, outcomes | 274 |
| `sim/cpu/ops.rs` | Instruction semantics | 462 |
| `sim/cpu/flags.rs` | Status flags and conditions | 179 |
| `sim/memory.rs` | Copy-on-write address spaces and protections | 428 |
| `sim/loader.rs` | Golden ELF images and the initial stack | 266 |
| `sim/corpus.rs` | Loads the golden programs and their manifests | 198 |
| `sim/client.rs` | The client that drives `DebuggerHandle` | 782 |
| `sim/oracles.rs` | Ground-truth checks | 570 |
| `sim/marks.rs` | Coverage marks | 113 |
| `sim/report.rs` | Traces, fingerprints, failures | 128 |
| `sim/conformance/tracee.rs` | The dual-run harness: one script, native and simulated | 478 |
| `sim/conformance/{cpu,kernel}.rs` | Lockstep and the kernel's rules (section 9) | 860 |
| `sim/tests.rs` | The gate's seeds, determinism, and sabotage tests | 110 |
| `src/bin/uscope-sim.rs` | Sweep and replay commands | 240 |
| `backend/linux/sim_edge.rs` | `SimTrace`, the simulated waiter, the controller facade, and ground truth | 819 |
| `backend/linux/native_tracee.rs` | The real traced process conformance tests drive | 302 |

The client and the oracles grew past their goals with the multi-threaded
checks; each still reads as one concern.

`SimTrace` lives in the facade rather than in `sim/`: `LinuxTraceOps` is
private to the Linux backend, so its implementation must be too. It only
translates; the kernel's semantics live in `sim/kernel`. P4 adds
`sim/kernel/debug_regs.rs`.

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
| K-EXEC-1 | A launched program first reports a stop for SIGTRAP with `si_code` `SI_USER` from itself, at its entry point, with `orig_rax` naming `execve`. Its `comm` is its file name cut to 15 bytes. |
| K-WAIT-1 | A thread has at most one reportable status. A thread woken out of a stop loses an unreported one. Ptrace requests on a thread not in a ptrace-stop fail with ESRCH. |
| K-WAIT-2 | Which ready status a wait returns is unspecified, so the scheduler chooses. One exception: a group leader's exit is reported after every other thread's. |
| K-TRAP-1 | `int3` raises SIGTRAP with `si_code` `SI_KERNEL` and `rip` after the trap byte, outside any system call (`orig_rax` is -1). A single step reports `TRAP_TRACE`. A step across `syscall` reports `TRAP_BRKPT` at the call's exit, with `orig_rax` naming the call. |
| K-SIG-1 | The tracer's `tgkill(SIGSTOP)` produces a signal-delivery stop with `si_code` `SI_TKILL` and the tracer's process as sender, whether the thread was running or stopped when it was sent. |
| K-INT-1 | `PTRACE_INTERRUPT` stops a running thread at its next kernel entry with `PTRACE_EVENT_STOP`. One sent to a thread that is already stopped stays pending until the thread resumes, and the thread's next stop of any kind consumes it. |
| K-INT-2 | `PTRACE_INTERRUPT` and `tgkill` return 0 for a thread at its exit event or an unreaped zombie, and ESRCH once it is reaped. When the reap lands inside the interrupt's own window, the interrupt fails with EIO (a fault, section 13). |
| K-EXIT-1 | `exit_group` with running siblings: every thread stops at `PTRACE_EVENT_EXIT`, with message `code << 8` and `si_code` `0x605`; the caller stops inside the call (`rax` is `-ENOSYS`, `orig_rax` names it). After `PTRACE_CONT`, each reports its exit, the leader's last. A thread ending alone with `exit` stops the same way. |
| K-EXIT-2 | Siblings held in any ptrace-stop, signal-delivery or event, are pulled out of them and stop at `PTRACE_EVENT_EXIT` too. One pulled from a clone event returns from the call on the way out. |
| K-EXIT-3 | SIGKILL from anywhere: every thread stops at `PTRACE_EVENT_EXIT` with message 9, then is reported killed by signal 9. |
| K-EXIT-4 | Once a group is exiting, a thread already at its exit stop is not released by SIGKILL. It answers `GETREGS` and memory reads, and waits for `PTRACE_CONT`. A thread at the exit stop of its own `exit` is released when its group starts exiting, by `exit_group` or SIGKILL: it finishes exiting without another stop. |
| K-EXIT-5 | A leader that exits alone stays a zombie until the last thread exits. Its `exe` link is gone, its maps read empty, ptrace requests on it fail with ESRCH, and `tgkill` still succeeds. Seizing a zombie fails with EPERM. |
| K-EXIT-6 | The thread that begins to exit last, before its exit stop, starts a group exit with its own status (the kernel's `synchronize_group_exit`). Once a group is exiting, every thread reaped reports the group's status, even one that exited alone earlier with another. A leader held at its exit stop therefore changes nothing; one that begins to exit last decides the status. |
| K-CLONE-1 | A creator tracing clones stops at `PTRACE_EVENT_CLONE` inside the call (`si_code` `0x305`, message the new thread's id, `rax` `-ENOSYS`); continuing it returns the id. The new thread is traced with the creator's options, starts where the creator returns, with `rax` zero and `orig_rax` naming `clone`, and first stops for a SIGSTOP with `SI_USER` from nobody. The two stops become reportable in either order. A clone by a thread not tracing clones is a model gap. |
| K-FORK-1 | A fork child gets a copy of the parent's address space, traps included. It is auto-attached with the parent's options and starts in a stop. |
| K-DR-1 | A watch hit raises SIGTRAP with `TRAP_HWBKPT` and `rip` after the instruction. DR6 changes only at debug exceptions and is stale at every other stop. |
| K-DR-2 | Single-stepping over a watched store gives one stop: `TRAP_TRACE`, with DR6 holding both the single-step bit and the watch bit. |
| K-DR-3 | New threads and fork children start with debug registers disarmed, but `PEEKUSER` of DR7 returns the creator's value. Detaching does not clear them. |
| K-DR-4 | Writing a debug-register address reserves a slot even while disabled, and can fail with ENOSPC. DR7 writes are transactional. |
| K-DR-5 | `rep stos` and `rep movs` trap once per iteration that touches the watched range, with `rip` still at the instruction. Stores of the same value trap. `POKEDATA` never traps. |
| K-MEM-1 | `PEEKDATA` and `POKEDATA` ignore page protections and fail only where nothing is mapped. CPU accesses obey protections and fault with `SEGV_MAPERR` or `SEGV_ACCERR`. |

**Probed so far** (`sim/conformance/kernel.rs`): K-EXEC-1, K-TRAP-1,
K-SIG-1, K-WAIT-1, K-WAIT-2 (the leader's exit is held back), K-EXIT-1 to
K-EXIT-6, K-CLONE-1, and K-MEM-1, each in its multi-threaded form where
one exists. Two P2 probes corrected the model before any session used it:
K-EXIT-4 had said a thread at its exit stop is never released, and the
leader's status had been modeled as its own until the last thread finished
exiting (K-EXIT-6). K-INT-1 and K-INT-2 are not modeled yet: a launched
process is stopped with `tgkill`, and `PTRACE_INTERRUPT` serves attached
processes, which P4 brings.

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
P4 adds what forking programs use. Any other syscall is a model gap.

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

As built in P1, a manifest records the toolchain, the hashes of the
sources and binaries, each variant's flags, and each run's arguments, exit
code, and output. A program's `arguments` file lists its runs, one argument
list per line; `golden-build` runs every variant with each and requires
them to agree. The other fields arrive with the programs that need them.
Threads interleave differently on every run, so `golden-build` runs each
binary 20 more times and fails if anything it prints or returns changes:
a program's behavior must not depend on scheduling, or transparency would
fail sessions for nothing.

**Size budget.** Each binary is a few kilobytes plus its DWARF. The whole
corpus stays under 2 MB in plain git.

**Initial programs.**

| Program | Exercises |
|---|---|
| `straight` | Loops, calls, recursion, and inlining in one thread. |
| `threads` | Workers created with raw `clone`, barriers, shared counters, and workers ending by `exit` and by `exit_group`. |
| `racing-exit` | `exit_group` while siblings run and hit breakpoints. |

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
or a status reaped inside a controller call, and each kind of SIGKILL.

## 14. Running the simulator

| Command | What it does |
|---|---|
| `just` | The gate: `golden-check`; the kernel and CPU conformance tests; 300 fixed seeds over every program and variant (about 0.3 s); a determinism double-run of the first 32 seeds; the coverage-mark check; and four sabotage tests showing the oracles catch lost trap writes, a deaf waiter, a thread resumed behind the controller's back, and a trap the CPU skips. |
| `just sim [SECONDS]` | A sweep: random seeds on every core for SECONDS (default 60), inside `scripts/contained.sh`. Failures are grouped by kind and check; each group keeps its smallest seed's report. |
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
- **Throughput**, measured in P2 on a 12-core Ryzen AI 9 HX 370 (4 Zen 5
  and 8 Zen 5c cores) over all three programs: about 1,400 sessions per
  second on one thread, about 9,600 per second in all on 12 threads, and
  about 9,100 on 24. A one-minute sweep runs over half a million sessions.
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
- Attach after an untraced run, with `PTRACE_INTERRUPT` (K-INT-1, K-INT-2)
  probed and modeled.
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
   2 MB (272 KB after P2). Should a larger budget ever be needed, the choice
   is between Git LFS and building in Nix with pinned hashes.
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
- **Swarm configuration:** the per-seed choice of which features and faults
  are active, and how intensely.
- **Tape:** the recorded sequence of a run's random draws, which can be
  replayed or shrunk.
