# uscope

uscope is a debugger for native programs on Linux x86-64, written in Rust.
Use it from your terminal, or from your editor through the Debug Adapter
Protocol. It debugs C, C++, Rust, Zig, Go, Odin, and Fortran.

## Features

- **Launch, attach, or inspect a crash.** Start a program, attach to one that
  is already running, or open a core dump.
- **Breakpoints and watchpoints.** Break on functions, lines, or addresses,
  with conditions. Watch a variable and stop when its value changes.
- **Stepping.** `step`, `next`, `stepi`, `nexti`, and `finish`, through inlined
  calls. Every thread stops and resumes together.
- **Stacks.** Backtraces through every loaded library, and each caller's
  variables as they were at its call.
- **Expressions.** One expression language for every supported language, with
  exact integer arithmetic.
- **Views.** Containers display as what they hold. A `Vec` shows its elements,
  for the C++, Rust, Go, Zig, and Odin standard libraries and for your own
  types.
- **Language runtimes.** Go goroutines and async Rust tasks show up as threads
  and stacks you can step through, and panics stop the program where they
  happen.
- **Honest output.** When a value is optimized out or memory cannot be read,
  uscope says so instead of guessing.

uscope also has a command-line interface with batch mode for scripts, a
browser UI (`uscope web`), and a Debug Adapter Protocol server for editors.

## Editors

uscope runs as a Debug Adapter Protocol server with `uscope dap`. It has a
VS Code extension and a working Neovim (nvim-dap) setup. Helix, Zed, and Emacs
(dape) are documented but not yet tested.

## Quick start

uscope builds inside a pinned Nix environment. With Nix installed:

```sh
just dev                    # enter the development environment
just build-test-programs    # build the sample programs
just run build/test-programs/basic
```

At the `(uscope)` prompt:

```text
break breakpoint_target
run
bt
finish
next
print first
print/x uscope_value
```

Other ways to start:

```sh
uscope ./program -- ARG...         # launch with arguments
uscope --attach PID                # attach; the process keeps running on exit
uscope --core core.1234            # open a core dump
uscope --batch -e 'break f' -e run -e bt ./program
uscope dap                         # serve DAP on stdio
```

## Learn more

- [Command line and REPL](docs/cli.md): flags, settings, and commands.
- [Expressions](docs/expressions.md): the expression language.
- [Views](docs/views.md): how containers are presented, and
  [writing your own](docs/writing-views.md).
- [Editors and DAP](docs/dap.md): setup for VS Code, Neovim, and others.
- [Go](docs/go.md) and [async Rust and tokio](docs/tokio.md): how uscope
  handles goroutines and tasks.
- [AGENTS.md](AGENTS.md): architecture and development rules for contributors.

## Development

```sh
just          # format check, Clippy, and the test suite
just test X   # the tests matching X
just stress   # the suite ten times under CPU load
just sim      # simulate random debugger sessions for 30 seconds
just all      # everything, before committing
```

See [AGENTS.md](AGENTS.md) for how the code is organized and tested.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE),
at your option.
