# uscope Development Guide

`uscope` is currently a Linux x86-64 native debugger written in Rust. Keep the implementation small and rigorous while preserving room for other operating systems, architectures, debug formats, and UI clients.

## Architecture

- Keep the public model and request/event protocol platform-neutral. ELF, DWARF, ptrace, and native register layouts belong at the edges.
- `ModuleImage` contains immutable static metadata. `LoadedModule` represents a runtime mapping. Never mix `ImageAddress` and `VirtualAddress`.
- UI clients use `DebuggerHandle`, bounded request channels, events, and immutable snapshots. Do not share mutable debugger state with CLI, TUI, DAP, or future UIs.
- All ptrace operations must execute on the dedicated controller OS thread that created the tracee. The waiter thread may call `waitpid` and send messages back; it must not call ptrace.
- Tokio coordinates asynchronous clients and message passing. Blocking process control remains on the controller thread.
- Debug-info providers normalize data at the boundary. The generic unwind loop iterates caller contexts; gimli owns DWARF CFI interpretation.
- Make unsupported states and partial results explicit. Never silently guess when doing so could produce a convincing but incorrect debugger result.
- Unsafe code is denied unless narrowly required. Every exception needs a safety comment and must satisfy the configured lints.

## Testing

- Practice test-driven development and prefer meaningful behavioral tests over numerous trivial assertions.
- Unit-test deterministic algorithms, invariants, boundaries, and typed failure modes only where code is very complicated. Use as few unit tests as possible, prefer tests that are higher leverage.
- Use `tests/support::Scenario` for real debugger workflows. It runs the public request/event path, records a transcript, applies deadlines, shuts down the debugger, and verifies the inferior was reaped.
- Replace superseded integration tests instead of retaining duplicate coverage.
- Keep CLI tests separate when they validate parsing, batch behavior, or rendered output rather than debugger semantics.
- Native fixtures live in `tests/fixtures`. Rust tests may launch them but must not invoke compilers. `just build-test-programs` builds them incrementally.
- Exercise a compact compiler/linker matrix where output can affect behavior: GCC and Clang, optimized and unoptimized, PIE and non-PIE, with and without frame pointers as relevant.
- Any lifecycle or concurrency change must test cleanup, cancellation, event/state consistency, and the absence of surviving inferior processes.

## Local Development

Enter the pinned Nix environment:

```sh
just dev
```

Run the complete local gate (default recipe):

```sh
just
```

Useful focused commands:

```sh
just test                          # incrementally build fixtures, then run tests
just build-test-programs           # build only native fixtures
cargo nextest run --test debugger  # run real debugger scenarios
just run build/test-programs/basic
```

Before committing, run formatting, aggressive Clippy, nextest, and doc tests via `just`. Keep comments concise and useful, document public APIs, group related Rust code with sensible whitespace, and avoid unrelated refactors.
