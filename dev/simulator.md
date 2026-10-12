# Simulator

The deterministic simulator in `src/sim` runs the real controller and
`DebuggerHandle` against a simulated kernel and CPU. After each action, oracles
compare debugger state with the world's ground truth.

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
