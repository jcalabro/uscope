# TODO

## Debugger

- Step into a chosen call on a line (DAP `stepInTargets`), and show a C, C++, Rust, or Zig function's return value after `finish`, as Go's is.
- Show the frames of tail calls in backtraces, as the chains entry values follow find them.

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
- Read debug files that share their information through dwz supplementary files (`.gnu_debugaltlink`), as distributions' debuginfod servers send.

## Later

- Sampling and instrumented profilers.
- Prometheus and OpenTelemetry collectors.
