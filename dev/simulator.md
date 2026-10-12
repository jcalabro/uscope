# Simulator

The deterministic simulator in `src/sim` runs the real controller and
`DebuggerHandle` against a simulated kernel and CPU. After each action, oracles
compare debugger state with the world's ground truth.

## Code map

| Path | What lives there |
| --- | --- |
| `src/sim/world.rs`, `machine.rs`, `schedule.rs` | Simulated world state, machine execution, and action scheduling |
| `src/sim/choices.rs`, `swarm.rs`, `corpus.rs` | Seeded choices, configuration variation, and saved inputs |
| `src/sim/kernel/` | Process, ptrace, signal, debug-register, perf, and syscall models |
| `src/sim/cpu/` | Simulated x86-64 operations and flags |
| `src/sim/client/` | Generated client behavior against `DebuggerHandle` |
| `src/sim/oracles.rs`, `audit.rs`, `marks.rs` | Ground-truth checks, invariants, and coverage marks |
| `src/sim/catches.rs`, `tracing.rs` | Catchpoint accounting and the BPF engine's tracepoint stream |
| `src/sim/conformance/` | Dual-run probes against the real Linux kernel and CPU |
| `src/backend/linux/sim_edge.rs` | Adapter from the real controller to simulated native operations |
| `src/bin/uscope-sim.rs`, `src/sim/report.rs` | Sweep/replay command and failure reports |
| `tests/golden/`, `scripts/golden.sh` | Reproducible native programs and manifest management |
| `plans/simulator.md` | Numbered model rules and design record |

## Determinism

One seed names one run for one commit. All choices come from the seed's
`Choices` streams. A simulated world must not read a clock, run a real thread,
or depend on hash-container iteration order.

`a_seed_always_names_the_same_run` checks this contract. Seeds are replay aids,
not regression tests, because code changes alter the run they name.

## Kernel model

Model a kernel behavior only after a dual-run conformance test pins it on the
real kernel. Record the behavior as a numbered rule in `plans/simulator.md`.
An unmodeled behavior is a model gap and must fail the run; do not guess.

## Oracles

Never weaken an oracle to make a run pass. Correct a faulty oracle in its own
change, explain the mistake, and add a unit test. A new oracle needs a sabotage
test that proves it catches the lie it was added to detect. A new feature needs
coverage marks reached by the gate's fixed seeds.

Classify a failure before fixing it:

- A debugger failure gets a failing test outside the simulator before its fix.
- A model gap gets a real-kernel probe and a numbered rule.
- A simulator failure is fixed in the simulator.

## Running it

```sh
just sim
just sim-seed SEED
just sim 600
```

The default sweep runs for 30 seconds. `just all` includes it. Use the ten
minute sweep before merging lifecycle, run-control, attach, concurrency, or
model changes. A sweep groups similar failures and reports the shortest run in
each group. Replay one and use `--at STEP` to inspect its state.

Golden programs under `tests/golden` are checked in as sources and manifests.
The pinned toolchain builds them under `build/golden`; hashes must match. Use
`just golden-record NAME` only for an intentional update, in a change of its
own.

## What the simulator runs

The simulator does not contain a second debugger. It runs the real Linux
controller state machine through `sim_edge`, replacing native system calls
with deterministic requests to a simulated world. Clients still use
`DebuggerHandle`, requests, replies, events, snapshots, and selection.

The world contains:

- processes, thread groups, parent relationships, and trace ownership;
- per-thread registers, wait status, signal state, and debug registers;
- virtual mappings, page protections, memory, and shadow provenance;
- a small x86-64 CPU for the instructions golden programs execute;
- modeled system calls and ptrace operations;
- perf sample production;
- scheduler choices and injected faults;
- ground-truth facts used only by oracles.

The controller must learn through the same edge operations it uses on Linux.
Never let it read world ground truth directly.

## Run structure

A seed initializes independent `Choices` streams. The swarm uses them to pick
a golden program, client action, scheduler decision, legal external event,
fault point, and data variation. Splitting streams prevents a new choice in
one area from needlessly perturbing unrelated areas.

Each step has a clear owner: client, controller, kernel, CPU, or external
world. The scheduler advances one enabled action. After the action settles,
oracles compare public and internal debugger state with ground truth.

The trace records choices, actions, native-edge calls, wait evidence, public
events, oracle checks, and the controller's flight recording. A replay must
not consult wall time or the host scheduler.

## Determinism hazards

The following are forbidden inside a simulated world:

- current time, randomized hash seeds, thread races, or host process IDs;
- filesystem enumeration whose order is not normalized;
- pointer addresses used as stable IDs;
- global random-number generators;
- iteration over unordered containers when order affects behavior;
- a choice whose available alternatives depend on logging or formatting.

Use ordered containers or sort before observation. Allocate simulated IDs from
world state. Draw every random decision from the named choice stream and print
it in the trace when it affects behavior.

## Kernel rules and conformance

The simulator models only facts established by dual-run tests on the real
kernel. Rules in `plans/simulator.md` cover exec, wait ordering, traps,
signals, seize and interrupt, exit, clone and fork, job control, debug
registers, ptrace memory, and perf sample layout.

A conformance test runs the same short operation script against Linux and the
model, then compares normalized observations. Normalize only facts the kernel
leaves unspecified, and state that freedom in the rule. Do not normalize away
a difference merely because it is inconvenient.

When new controller code calls an unmodeled native operation, simulation must
fail with a model gap that names the operation and action. The response is:

1. write the real-kernel probe;
2. state the numbered rule;
3. add the model behavior;
4. add or update an oracle if the fact is publicly observable;
5. rerun the failing seed, then sweep.

## CPU and memory

The CPU implements only instructions present in the golden corpus and needed
to reach modeled behavior. An unknown instruction is a model gap. Do not
decode it as a no-op.

CPU memory obeys mapping permissions and produces modeled faults. Ptrace
memory accesses follow ptrace rules and can read or write despite ordinary
page protection where Linux does. Keep these paths separate.

Shadow state records what bytes and registers mean in the ground truth. It
lets oracles distinguish a value the program established from coincidental
bytes. Debugger code cannot read shadow state.

Mappings track file-backed and anonymous bytes, relocation, stack limits, and
page presence needed by perf stack copies. Changes should mirror a numbered
kernel rule or fail as a gap.

## Golden corpus

Golden sources are tiny programs compiled by the pinned toolchain. Their
manifests record exact binary hashes and extracted facts used by the model.
Checking in sources makes intent reviewable; checking hashes prevents a tool
or flag change from silently changing every simulated run.

Each program should add one useful behavior or interaction. Prefer extending a
small program over adding a large application. Keep loops and memory bounded
so a seed cannot exhaust the machine.

`just build-test-programs` verifies every manifest. If a deliberate source or
toolchain change alters a binary, use `just golden-record NAME`, inspect the
new facts and disassembly, and commit the manifest update separately from a
debugger fix where possible.

## Faults and external events

Fault injection represents events Linux can really produce: process death at
a preemption point, group exit during stop handling, trace ownership races,
debug-register failures, and similar conditions.

Every fault has a legal window and source. The simulator must not invent an
errno or ordering with no kernel evidence. Fault choice zero should normally
mean the simplest no-fault path so shrinking moves toward a smaller run.

External events are scheduled between controller operations, not in the
middle of an operation the native contract treats as atomic.

## Oracles

An oracle states one invariant and compares independent facts. Examples
include:

- every public live thread corresponds to a world thread owned by the trace;
- a public all-stop has no running live tracee;
- pending signals and reported stop reasons match kernel evidence;
- installed breakpoints preserve program bytes and owner counts;
- selected frames and registers agree with CPU and shadow state;
- memory reads hide trap bytes and report unavailable ranges;
- cleanup leaves no owned tracees, replies, or worker state;
- profiler samples decode to the world stack and mapping active at capture.

An oracle must not call the implementation helper that computes the value it
checks. That proves consistency with itself, not correctness.

A sabotage test introduces the exact lie the oracle is meant to catch and
asserts that it fails with a useful message. Keep sabotage hooks narrow and
test-only.

## Coverage marks

Marks record semantic events reached by a run: a co-hit repair, a signal
during all-stop gathering, an attach race, an exit phase, a watchpoint overlap,
or another feature branch. Fixed gate seeds must cover required marks.

Marks are not line coverage. Add one when the behavior has a meaningful name
and an oracle or test expectation. Do not add marks merely to make a percentage
larger.

## Shrinking failures

Choice encoding is designed so simpler alternatives have smaller values. A
failure report groups traces by a stable fingerprint and keeps the shortest
representative. Replay that seed before making changes:

```sh
just sim-seed SEED
just sim-seed SEED --at STEP
```

The seed is diagnostic only for the commit that produced it. Turn a debugger
failure into a focused non-simulator regression test before fixing it. The
test should express the public behavior without depending on the random run.

## Failure classification

| Failure | Evidence | Required response |
|---|---|---|
| Debugger bug | Model and oracle follow a probed rule; public state disagrees | Add a failing public regression test, then fix debugger code |
| Model gap | An operation, instruction, syscall, errno, or ordering is not modeled | Probe Linux, add a numbered rule, then implement it |
| Simulator bug | World, scheduler, shadow state, trace, or oracle violates its own contract | Fix simulator code with a direct test |
| Oracle bug | The comparison asks for a fact Linux or the public contract does not promise | Correct it in a separate change and add a test for the corrected rule |
| Golden drift | Built bytes differ from a manifest | Find the source or toolchain cause; record only if deliberate |

Do not weaken an oracle, add retries, or retain a seed as a test to make a
sweep green.

## Extending the simulator

For a new debugger feature:

1. Decide whether it uses modeled kernel or CPU behavior.
2. Add conformance probes for every new external fact.
3. Extend native and simulated edge contracts together.
4. Add world state with explicit ownership and deterministic IDs.
5. Add client actions that can reach the feature.
6. Add coverage marks for important branches.
7. Add an independent oracle and sabotage test.
8. Run focused simulator tests, replay failures, then `just sim 600`.

If the feature is pure inspection over already modeled state, it may need only
client actions and oracles. If it adds a ptrace request or kernel event, it
needs conformance first.
