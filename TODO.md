# TODO List

## Debugger core

- [ ] Add remote debugging support.
- [x] Add watchpoints.
- [ ] Add value-change-only watchpoints by evaluating hits at an internal stop and resuming transparently, shared with conditional breakpoints.
- [ ] Add conditional and hit-count breakpoints.
- [ ] Add disassembly support.
- [ ] Add caller-frame selection for variables and source context, especially for core dumps.
- [ ] Symbolize frames in modules without DWARF from their ELF symbol tables.
- [ ] Support core dumps from other machines with a sysroot or library search path.
- [ ] Expand expression evaluation beyond structural value inspection.
- [ ] Improve support for advanced DWARF location expressions and composite locations.

## Client interfaces

- [ ] Add a debugger protocol server, starting with DAP support.
- [ ] Add a richer interactive or TUI client.
- [ ] Keep the public request/event model suitable for multiple clients.

## Language and platform support

- [ ] Expand Go execution control, goroutine awareness, and composite value rendering.
- [ ] Add first class support for tokio as much as it will allow
- [ ] Add broader libc support for TLS.
- [ ] Add additional Linux architectures.
- [ ] Establish the platform abstraction needed for other operating systems.

## Reliability and maintainability

- [ ] Continue expanding lifecycle, concurrency, signal, and cleanup coverage.
- [ ] Improve diagnostics for unsupported and malformed debug metadata.
- [ ] Review and harden behavior around `exec`, dynamic modules, and unusual native stops.
- [ ] Keep the compiler and debugger compatibility matrix current.
- [ ] Expand the e2e test system, including more happy and sad paths
- [ ] Add continuous integration for all supported architectures
- [ ] Add e2e tests against some real, well-known open source programs that run in CI

## User experience

- [ ] Add richer breakpoint management and inspection commands.
- [ ] Improve source navigation and stop presentation.
- [ ] Document the public debugger API and supported feature matrix.

## Larger Next-Steps

- [ ] OTEL collector
- [ ] Prometheus collector
- [ ] Sampling profiler
- [ ] Instrumented profiler
- [ ] Rich web ui
