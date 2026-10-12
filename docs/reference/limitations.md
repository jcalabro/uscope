# Known limits

This page states the broad support boundary. Individual commands can have
narrower limits and report them when used.

## Host and target

- The debugger runs on Linux x86-64.
- It debugs native x86-64 Linux processes and core dumps.
- Cross-architecture and remote targets are not supported.
- Process control depends on Linux ptrace permissions.

## Debug information

- Source, types, variables, and unwinding depend on the information emitted by
  the compiler and linker.
- Optimized programs may have missing variables, non-linear source locations,
  inlined frames, and values available for only part of a function.
- Separate debug files and debuginfod are supported when they can be matched
  to a module.
- A partial value or stack stays partial; uscope does not fill missing state
  with assumed values.

## Run control

- Public debugging is all-stop. One thread stopping causes every live thread
  to stop before state is published.
- Hardware watchpoint count, size, and alignment are limited by x86-64 debug
  registers.
- Source stepping is only as precise as the compiler's line and inline-call
  information.
- Core dumps are read-only.

## Languages and runtimes

- C, C++, Rust, Go, Zig, Odin, Fortran, D, Nim, and Ada use the shared native
  debugger.
- Logical runtime tasks are implemented for Go goroutines and tokio tasks.
- Runtime support follows known compiler and runtime layouts. Unknown versions
  can leave tasks or logical frames unavailable.
- Built-in views cover selected standard-library and runtime types, not every
  third-party container.

## Clients

- The terminal, browser, and DAP clients share the same debugger state.
- The repository's VS Code and Neovim paths are tested during development.
- Helix, Zed, and Emacs configurations are documented as DAP clients but do
  not yet have the same acceptance coverage.
- A browser session has no built-in multi-user authorization. Access is
  controlled by its listener, tunnel, or reverse proxy.

## Distribution

uscope does not yet publish a supported package or stable release artifact.
The repository development environment is not an end-user installation path.
