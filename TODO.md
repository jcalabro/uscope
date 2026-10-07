# TODO

## Debugger

- Evaluate CFI expression rules (`DW_CFA_expression` and `DW_CFA_val_expression`).
- Synthesize `name@plt` symbols for PLT stubs.
- Break on symbol names in modules without DWARF.
- Show symbol versions where they distinguish otherwise identical names.
- Separate debug information: `.gnu_debuglink`, build-id directories, and debuginfod.
- Write registers, for assigning `$rax` and DAP's `goto` (VS Code's Jump to Cursor).
- Step into a chosen call on a line (DAP `stepInTargets`), and show a C, C++, Rust, or Zig function's return value after `finish`, as Go's is.
- Show the frames of tail calls in backtraces, as the chains entry values follow find them.
- Model function types, so function pointers show their signature.
- Accept C base type names in casts, such as `(unsigned char)x`, when the program's debug information lacks them.

## Languages and platforms

- First-class tokio support.
- Other Linux architectures, then other operating systems.

## Clients

- A richer interactive or TUI client.
- Richer breakpoint management and stop presentation in the CLI.

## Reliability

- Continuous integration on every supported architecture.
- End-to-end tests against well-known open source programs.
- Harden `exec`, dynamic modules, and unusual native stops.
- Better diagnostics for unsupported and malformed debug metadata.

## Later

- A web UI.
- Sampling and instrumented profilers.
- Prometheus and OpenTelemetry collectors.
