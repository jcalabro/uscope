# uscope Development Guide

`uscope` is currently a Linux x86-64 native debugger written in Rust. Keep the implementation small and rigorous while preserving room for other operating systems, architectures, debug formats, and UI clients.

## Architecture

- Keep the public model and request/event protocol platform-neutral. ELF, DWARF, ptrace, and native register layouts belong at the edges.
- `ModuleImage` contains immutable static metadata. `LoadedModule` represents a runtime mapping. Never mix `ImageAddress` and `VirtualAddress`.
- UI clients use `DebuggerHandle`, bounded request channels, events, and immutable snapshots. Do not share mutable debugger state with CLI, TUI, DAP, or future UIs.
- All ptrace operations must execute on the dedicated controller OS thread that created the tracee. The waiter thread may call `waitpid` and send messages back; it must not call ptrace.
- Tokio coordinates asynchronous clients and message passing. Blocking process control remains on the controller thread.
- The public debugger is all-stop. The Linux edge still tracks each tracee independently, classifies raw wait events before publishing them, preserves pending signals per thread, and publishes a stop only after every live thread is known stopped.
- Run-control requests are acknowledged before their eventual stop events. Every stopped-state mutation carries a `StopId`; stale clients must fail instead of controlling a newer stop.
- Software breakpoint sites belong to the process address space. Keep physical installation separate from user and execution-plan ownership, hide trap bytes from user memory reads, and repair co-hit threads sequentially while siblings remain stopped.
- Debug-info providers normalize data at the boundary. The generic unwind loop iterates caller contexts; gimli owns DWARF CFI interpretation.
- Resolve source paths while loading debug metadata, but read source contents lazily outside the ptrace controller thread.
- A load seals a module's debug information as one validated image of tables (`src/image`, `plans/performance.html`); `ModuleImage` and the providers read it through views. The module's path, identifier, and separate debug file's location belong to its binding, never to the image's bytes, so one image serves every binding and the image cache (`src/cache.rs`) can store it. A cache entry's key names its inputs' bytes and the sources the loader was built from.
- Expressions are one neutral language for every source language (`docs/expressions.md`, `plans/expressions.md`). `src/eval` is pure: it reaches a program only through traits the debugger implements, and a boundary test keeps process control, debug information, I/O, clocks, and threads out of it. Parsing needs no program; binding resolves names in one frame scope; running reads one validated stop.
- Integer arithmetic is exact, bit operations keep their operand's width, and only casts truncate. Data layout belongs to the debug-info providers; the evaluator never computes an offset. Every example in `docs/expressions.md` runs as a test, so change the reference and the implementation together.
- Make unsupported states and partial results explicit. Never silently guess when doing so could produce a convincing but incorrect debugger result.
- Unsafe code is denied unless narrowly required. Every exception needs a safety comment and must satisfy the configured lints.

## Testing

- Practice test-driven development and prefer meaningful behavioral tests over numerous trivial assertions.
- Unit-test deterministic algorithms, invariants, boundaries, and typed failure modes only where code is very complicated. Use as few unit tests as possible, prefer tests that are higher leverage.
- Use `tests/support::Scenario` for real debugger workflows. It runs the public request/event path, records a transcript, applies deadlines, shuts down the debugger, and verifies the inferior was reaped. Use `support::ScratchDir` for temporary files and `support::ExternalProcess` for attach targets so nothing outlives a failing test. `Scenario::launch` reads a fixture's debug information once per test process and shares it among that test's scenarios; use `Scenario::new` for a program the test changes.
- Run integration tests with nextest, which gives each test its own process: one process can trace with only one live session at a time.
- `just test` reads the images `just cache-warm` writes to `target/image-cache`; `just stress` loads afresh, so the loader itself still runs the whole suite. Nothing a test runs may use the user's cache: under nextest the cache is off unless `USCOPE_CACHE_DIR` names one, and tests of the cache open their own in a scratch directory.
- Replace superseded integration tests instead of retaining duplicate coverage.
- Keep CLI tests separate when they validate parsing, batch behavior, or rendered output rather than debugger semantics.
- Native fixture sources live in language directories under `tests/fixtures`; each Go executable has its own package subdirectory. Scenario filenames describe the program without repeating the language. Rust tests may launch fixtures but must not invoke compilers. `just build-test-programs` builds them incrementally, compiling at once on every CPU (`USCOPE_FIXTURE_JOBS=1` builds one at a time). A step of `scripts/build-test-programs.sh` that reads what it built must wait for it first, as its helpers do: `wait_for` the outputs it names, or `wait_builds` for every running compile. The tokio fixtures build in a shell of their own beside the rest.
- Exercise a compact compiler/linker matrix where output can affect behavior: GCC and Clang, optimized and unoptimized, PIE and non-PIE, with and without frame pointers as relevant.
- Any lifecycle or concurrency change must test cleanup, cancellation, event/state consistency, and the absence of surviving inferior processes.
- Execution-control changes should cover the pure reducer/classifier where applicable, the public scenario harness, and synchronized native fixtures.
- A test may only wait for something it can observe: an event, a debugger state, a `/proc` fact, or a line the fixture prints. Never assume something has happened by now. Before pausing, signalling, or attaching, wait until the program has visibly reached the point the assertions depend on; a poll that reaches its deadline fails rather than carrying on; never assert that something did not happen within a window. Fixtures synchronize their own threads instead of sleeping.
- Tests must not be able to exhaust the machine. Every test process aborts once its live heap passes 1 GiB (`tests/support/memory_cap.rs`), `just test` and `just stress` run inside a memory-capped scope, and every loop that collects output consumes input on each pass. Run fuzzers, sweeps, and wide runs only through `scripts/contained.sh` or the `just` recipes that use it.
- Keep iteration fast. While developing, run only checks that finish in seconds: the tests the change touches, selected by name, and a build or Clippy of what changed. Run the full gate, `just stress`, and other expensive checks once, at the end of a large body of work, never after each fix. Write each new test before its fix and watch it fail, which proves it catches the bug; never prove that afterwards by reverting the fix and rebuilding.
- Run `just stress` before merging any lifecycle, run-control, attach, or concurrency change. It runs the suite ten times, about a minute, with twice as many test threads as CPUs and busy loops beside them; races that fail one run in hundreds when idle fail several times as often under that load. Give a count and a nextest filter to chase one failure, e.g. `just stress 100 -E 'binary(dap)'`. It stops at the first failure because a later pass of the same test would remove that test's flight recording.

## Flight Recorder

Development builds (`debug_assertions`) record every client request, ptrace control call, wait status, stop classification, published event, and panic, one timestamped line each, under `target/flight-recorder`. Release builds compile none of it. Read a recording before adding temporary tracing to diagnose a failure.

- A failing test keeps its recordings in `tests/<suite>/<test>.log` (scenarios) and `<test>.adapter.log` (DAP adapters), and prints their paths; passing tests leave nothing. The directory therefore lists the tests that failed when last run.
- Each `uscope` run streams to `runs/`, keeping the latest 20, and `latest.log` links to the newest. `USCOPE_FLIGHT_RECORDING=PATH` streams to PATH instead, and an empty value turns recording off.
- Record new native control paths through `record!` or the `Recorded` ptrace wrapper. Recording must never change what the inferior sees.

## Simulator

The deterministic simulator (`src/sim`, `plans/simulator.md`) runs the real controller and `DebuggerHandle` against a simulated kernel and CPU, so one seed names one complete, reproducible session, and oracles check the debugger against the simulation's ground truth after every action.

- Keep runs deterministic. All randomness comes from the seed's `Choices` streams; nothing reads the clock, iterates a hash container, or runs a real thread inside a world. `a_seed_always_names_the_same_run` checks this.
- Model a kernel behavior only once a dual-run test in `src/sim/conformance/kernel.rs` pins it on the real kernel, and number it as a rule in the plan. Whatever is not modeled fails the run as a model gap; never guess.
- Never loosen an oracle to make a run pass. When an oracle is wrong, correct it in a change of its own that says why, with a unit test. A new oracle gets a sabotage test showing it catches the lie it exists for, and a new feature gets coverage marks that the gate's fixed seeds must reach.
- A failure's kind decides the response. A debugger failure gets a test outside the simulator, written to fail first, before the fix; a model gap gets a probe and a rule; a simulator failure is fixed in the simulator. Seeds name runs only for the commit they ran on, so they are never kept as tests.
- `just all` sweeps for 30 seconds before every commit. Before merging a lifecycle, run-control, attach, or concurrency change, or a change to the model, sweep longer: `just sim 600`. A sweep groups failures by what their messages share and reports each group's shortest run; replay it with `just sim-seed SEED`, and see the state at an action with `--at STEP`. The trace includes the controller's flight recording.
- The golden programs in `tests/golden` are checked in as sources and manifests. `just build-test-programs` builds them into `build/golden` with the pinned toolchain and fails unless every binary matches the hash its manifest records. Re-record a manifest with `just golden-record NAME` only on purpose, in a commit of its own.

## Web UI

`uscope web` (`src/web`, `plans/web-ui.html`) serves a React page (`web/`) to every tab that joins one session over a WebSocket. Like the CLI and DAP, the server is a `DebuggerHandle` client.

- The server sends tabs state and notices; tabs send requests that name the `StopId` they read. Cache answers for a stop by request; forget them on writes, setting changes, and session changes.
- Formatting lives in `src/present` and is shared with the CLI. Do not copy it into the page.
- `web/src/protocol.gen.ts` is generated from `src/web/protocol.rs` with ts-rs. A test fails when it is stale; regenerate it with `USCOPE_UPDATE_PROTOCOL=1`.
- Debug builds read the built page from `build/web` at run time, so `just web` is enough after a page change. Release builds embed it.
- The page's address bar holds the tab's whole view (stop, thread or task, frame, source, view, memory). A shared link opens the same view.
- `just web-test` runs tsc, biome, and Vitest in seconds. `just web-e2e` runs Playwright against real servers in Chromium and Firefox; pass `--project=chromium -g NAME` for a single test. Rerecord the replay transcripts with `just web-transcripts`.
- To see the page yourself, run `just web-probe PROGRAM STEPS…`. It saves a screenshot after each step and prints the console. Steps are listed in `web/e2e/probe.ts`. `just web-dev` serves the page with live reload.
- Use the tools in `web/node_modules/.bin` or the Nix shell's `biome`. Never use `npx`, which downloads an unrelated package when a name is missing.

## Local Development

Run all project commands inside the pinned Nix environment. Do not run `cargo`,
`just`, compilers, linters, or tests directly from the host environment. For
non-interactive use, wrap the command with `just dev --command`, for example
`just dev --command just`.

From the nix environment (activated via direnv), run the complete local gate (default recipe):

```sh
just
```

Useful focused commands:

```sh
just build-test-programs           # build native fixtures and golden programs
just test                          # run the tests
just stress                        # run the tests ten times under CPU load
cargo nextest run --test debugger  # run real debugger scenarios
just run build/test-programs/basic
just sim                           # simulate random sessions for 30 seconds
just sim-seed SEED                 # replay one simulated session
just golden-record NAME            # re-record a golden program's manifest
```

Before committing, run `just all`: formatting, aggressive Clippy, nextest, doc tests, `just stress`, and a simulator sweep. `just` alone runs the faster gate without stress or the sweep. Keep comments concise and useful, document public APIs, group related Rust code with sensible whitespace, and avoid unrelated refactors.
