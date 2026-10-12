# Testing

Tests protect behavior, invariants, and cleanup. Prefer a small number of
high-value scenarios over assertions that restate implementation details.

## Workflow

Write the test before the fix and run it while the bug is present. This proves
that the test catches the problem. Do not prove it later by reverting a
completed fix.

Run focused tests while developing. Run the full gate once the change is
ready. Expensive sweeps and stress runs should not follow every edit.

## Debugger scenarios

Use `tests/support::Scenario` for real debugger workflows. It uses the public
request/event path, records a transcript, applies deadlines, shuts the
debugger down, and verifies that the inferior was reaped.

`Scenario::launch` reads a fixture's debug information once per test process
and shares it among that test's scenarios. Use `Scenario::new` when the test
changes the program.

Use `support::ScratchDir` for temporary files and `support::ExternalProcess`
for attach targets. A failed test must not leave a process or file lifecycle
unmanaged.

Run integration tests with nextest. A test process may have only one live
tracing session.

## Synchronization

A test may wait only for something it can observe: an event, debugger state, a
`/proc` fact, or a fixture output line. Polls must fail at a deadline. Do not
sleep and assume the inferior reached a point, or assert that an event did not
happen within an arbitrary window.

Fixtures synchronize their own threads. Before attaching, pausing, or sending
a signal, wait for the fixture to expose the state the assertion needs.

## Fixtures

Native sources live in language directories under `tests/fixtures`. Each Go
executable has its own package directory. Scenario filenames describe the
program without repeating the language.

Rust tests launch built fixtures; they do not invoke compilers. Build fixtures
with `just build-test-programs`. The script builds in parallel. Any step that
reads an output must first call `wait_for` for those outputs or `wait_builds`
for all active compiles.

Use a compact compiler and linker matrix when generated code changes behavior:
GCC and Clang, optimized and unoptimized, PIE and non-PIE, and frame pointers
on and off where relevant.

## Bounds

Every test process aborts if its live heap exceeds 1 GiB. Tests and stress runs
also execute in memory-capped scopes. Every output-collection loop must consume
input on each pass.

Run fuzzers, broad parameter sweeps, and wide runs only through
`scripts/contained.sh` or a Just recipe that uses it.

## Flight recorder

Development builds record client requests, ptrace calls, wait statuses, stop
classification, events, and panics under `target/flight-recorder`. Read the
recording before adding temporary logging.

A failing scenario keeps `tests/<suite>/<test>.log`; DAP tests may also keep an
adapter log. Ordinary runs stream under `runs/`, retain the latest 20, and
update `latest.log`. Set `USCOPE_FLIGHT_RECORDING=PATH` to choose a path or to
an empty value to disable recording.

Record new native control paths with `record!` or the `Recorded` ptrace
wrapper. Recording must never change what the inferior observes.
