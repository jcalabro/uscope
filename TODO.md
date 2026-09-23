# TODO List

## Debugger core

- [ ] Add attach-to-process support.
- [ ] Add core dump debugging.
- [ ] Add remote debugging support.
- [ ] Add watchpoints.
- [ ] Add conditional and hit-count breakpoints.
- [ ] Add disassembly support.
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
