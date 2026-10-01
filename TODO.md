# TODO List

## Debugger core

- [x] Add watchpoints.
- [x] Symbolize frames in modules without DWARF from their ELF symbol tables.
- [x] Add disassembly support, with an `info symbol <address>` command built on the same address resolution.
- [x] Name indirect branch targets by reading their memory operand at the stop, such as GOT slots.
- [ ] Support core dumps from other machines with a sysroot or library search path.
- [ ] Add caller-frame selection for variables and source context, especially for core dumps.
- [ ] Add hit-count breakpoints by evaluating hits at an internal stop and resuming transparently.
- [ ] Add value-change-only watchpoints using the same internal evaluate-and-resume stop.
- [ ] Expand expression evaluation beyond structural value inspection.
- [ ] Add conditional breakpoints.
- [ ] Improve support for advanced DWARF location expressions and composite locations.
- [ ] Register the vDSO as a memory-backed module so backtraces unwind through it and name its frames.
- [ ] Unwind through signal trampolines: evaluate CFI expression rules and present the trampoline frame itself as the signal frame.
- [ ] Synthesize `name@plt` symbols for PLT stubs.
- [ ] Set breakpoints by symbol name in modules without DWARF.
- [ ] Show symbol versions where they distinguish otherwise identical names.
- [ ] Load separate debug information through `.gnu_debuglink`, build-id directories, and debuginfod.

## Client interfaces

- [ ] Add a debugger protocol server, starting with DAP support.
- [ ] Add a richer interactive or TUI client.
- [ ] Keep the public request/event model suitable for multiple clients.

## Language and platform support

- [ ] Expand Go execution control, goroutine awareness, and composite value rendering.
- [ ] Add broader libc support for TLS.
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
