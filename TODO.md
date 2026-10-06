# TODO

## Debugger

- The rest of the DWARF expression language: entry values, parameter references, and implicit pointers in pieces.
- Register the vDSO as a memory-backed module, so backtraces unwind through it and name its frames.
- Unwind through signal trampolines: evaluate CFI expression rules and present the signal frame.
- Synthesize `name@plt` symbols for PLT stubs.
- Break on symbol names in modules without DWARF.
- Show symbol versions where they distinguish otherwise identical names.
- Separate debug information: `.gnu_debuglink`, build-id directories, and debuginfod.
- Write registers, for assigning `$rax` and DAP's `goto` (VS Code's Jump to Cursor).
- Step into a chosen call on a line (DAP `stepInTargets`) and show a function's return value after `finish`.
- Model function types, so function pointers show their signature.
- Accept C base type names in casts, such as `(unsigned char)x`, when the program's debug information lacks them.
- Follow fork children, and offer them to DAP clients as child sessions (`startDebugging`).

## Languages and platforms

- Go: source stepping, goroutines, split-stack backtraces, and composite values.
- TLS for libcs other than glibc, such as musl.
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

- Sampling and instrumented profilers.
- A web UI.
- Prometheus and OpenTelemetry collectors.
