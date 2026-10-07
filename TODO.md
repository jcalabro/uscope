# TODO

## Debugger

## Languages and platforms

- First-class tokio support.
- Other Linux architectures, then other operating systems.

## Clients

- A richer interactive or TUI client.

## Reliability

- Continuous integration on every supported architecture.
- End-to-end tests against well-known open source programs.
- Harden `exec`, dynamic modules, and unusual native stops.
- Better diagnostics for unsupported and malformed debug metadata.
- Read debug files that share their information through dwz supplementary files (`.gnu_debugaltlink`), as distributions' debuginfod servers send.

## Later

- Sampling and instrumented profilers.
- Prometheus and OpenTelemetry collectors.
