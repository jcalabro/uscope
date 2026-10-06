# TODO List

## Debugger core

- [x] Add watchpoints.
- [x] Symbolize frames in modules without DWARF from their ELF symbol tables.
- [x] Add disassembly support, with an `info symbol <address>` command built on the same address resolution.
- [x] Name indirect branch targets by reading their memory operand at the stop, such as GOT slots.
- [x] Support core dumps from other machines with a sysroot or library search path.
- [x] Map recorded source paths to local directories for programs built elsewhere.
- [x] Add caller-frame selection for variables and source context, especially for core dumps.
- [x] Add hit-count breakpoints by evaluating hits at an internal stop and resuming transparently.
- [x] Add value-change-only watchpoints using the same internal evaluate-and-resume stop.
- [x] Expand expression evaluation beyond structural value inspection.
- [x] Add conditional breakpoints.
- [ ] Add conditions and hit conditions to watchpoints; DAP refuses them on data breakpoints today.
- [ ] Improve support for advanced DWARF location expressions and composite locations.
- [ ] Register the vDSO as a memory-backed module so backtraces unwind through it and name its frames.
- [ ] Unwind through signal trampolines: evaluate CFI expression rules and present the trampoline frame itself as the signal frame.
- [ ] Synthesize `name@plt` symbols for PLT stubs.
- [ ] Set breakpoints by symbol name in modules without DWARF.
- [ ] Show symbol versions where they distinguish otherwise identical names.
- [ ] Load separate debug information through `.gnu_debuglink`, build-id directories, and debuginfod.

## Client interfaces

- [x] Add a debugger protocol server, starting with DAP support.
- [x] Serve the expression language through DAP: console expressions and assignments, hovers, completions, value and declaration locations, and session-wide hexadecimal.
- [x] Give the VS Code extension hovers, inline values, hexadecimal display, offered programs, and a user acceptance test of happy and sad paths.
- [ ] Write registers, for assigning `$rax` and for DAP's `goto` (VS Code's Jump to Cursor).
- [ ] Step into a chosen call on a line (DAP `stepInTargets`), and show a function's return value after stepping out of it.
- [ ] Model function types, so function pointers show their signature instead of `DwTag(21) *`.
- [ ] Accept C base type names in casts, such as `(unsigned char)x`, when the program's debug information has no such type.
- [ ] Follow fork children, and offer them to DAP clients as child sessions (`startDebugging`).
- [ ] Add a richer interactive or TUI client.
- [ ] Keep the public request/event model suitable for multiple clients.

## Language and platform support

- [ ] Expand Go execution control, goroutine awareness, and composite value rendering.
- [x] Locate TLS in a glibc of another version than the debugger's `libthread_db`.
- [ ] Add broader libc support for TLS, such as musl.
- [ ] Add first class support for tokio as much as it will allow
- [ ] Add additional Linux architectures.
- [ ] Establish the platform abstraction needed for other operating systems.

## Reliability and maintainability

- [ ] Add continuous integration for all supported architectures
- [ ] Keep the compiler and debugger compatibility matrix current.
- [ ] Expand the e2e test system, including more happy and sad paths
- [ ] Continue expanding lifecycle, concurrency, signal, and cleanup coverage.
- [ ] Review and harden behavior around `exec`, dynamic modules, and unusual native stops.
- [ ] Improve diagnostics for unsupported and malformed debug metadata.
- [ ] Add e2e tests against some real, well-known open source programs that run in CI

## User experience

- [ ] Add richer breakpoint management and inspection commands.
- [ ] Improve source navigation and stop presentation.
- [ ] Document the public debugger API and supported feature matrix.

## Larger Next-Steps

- [ ] Sampling profiler
- [ ] Instrumented profiler
- [ ] Rich web ui
- [ ] Prometheus collector
- [ ] OTEL collector
