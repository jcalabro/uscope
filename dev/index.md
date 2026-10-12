# Developing uscope

This directory documents how to change uscope. It stays in the repository and
is not part of the public documentation website.

Start with [architecture](architecture.md), then read the page for the area you
will change:

- [Process control](process-control.md)
- [Debug information](debug-information.md)
- [Testing](testing.md)
- [Simulator](simulator.md)
- [Performance](performance.md)
- [Web UI](web-ui.md)
- [Documentation](documentation.md)

## Development environment

Every project command runs in the pinned Nix environment. Enter it with:

```sh
just dev
```

For scripts and agents, wrap one command:

```sh
just dev --command just
just dev --command just test EXPRESSION
```

The shell is pure. Tools, compilers, fixtures, and tests must not depend on
host paths or packages. Add a missing tool to `flake.nix`.

## Common commands

```sh
just build-test-programs
just test NAME
just web-test
just sim
just stress
just all
```

Use focused checks while editing. `just all` is the final local gate.

## Repository map

| Path | Contents |
|---|---|
| `src/model`, `src/protocol.rs` | Public model and debugger protocol |
| `src/backend` | Native process-control edge |
| `src/debug_info` | ELF, DWARF, Go metadata, and architecture providers |
| `src/image` | Validated immutable debug-information tables |
| `src/eval` | Pure expression parser, binder, and evaluator |
| `src/view` | View parser, binding, and presentation |
| `src/profiler`, `src/analysis` | Recording and profile analysis |
| `src/sim` | Deterministic debugger simulator |
| `src/cli`, `src/dap`, `src/web` | User-interface clients |
| `web` | Browser client |
| `tests` | Public debugger, DAP, runtime, and web scenarios |
| `views` | Built-in views and visualizers |
| `sdk` | Headers and libraries used by debugged programs |
| `plans` | Design decisions and implementation records |
