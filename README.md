# uscope

uscope is a native debugger for Linux x86-64, written in Rust. It debugs C,
C++, Rust, Zig, and Go programs from a terminal REPL or, through the Debug
Adapter Protocol, from VS Code, Neovim, Helix, Zed, and Emacs.

## Features

- **Targets**: launch a program, attach to a running process, or open a core
  dump, including one from another machine (`--sysroot`, `--module-path`).
- **Breakpoints** on functions, lines, and addresses, with
  [expression](docs/expressions.md) conditions and hit conditions, including
  in shared libraries that load later.
- **Hardware watchpoints** that stop on a value change, on every store, or on
  any access, with conditions and hit conditions, scoped to the lifetime of
  the storage they watch.
- **Execution control**: continue, `step`, `next`, `stepi`, `nexti`, and
  `finish`, through inlined calls, across all threads (all-stop).
- **Stacks**: backtraces through every loaded module, with frame selection
  that shows each caller's variables as they were at its call.
- **Values**: one [expression language](docs/expressions.md) for every source
  language, with exact integer arithmetic, casts, and assignment.
- **[Views](docs/views.md)** that show containers as what they stand for, such
  as a `Vec` as its elements, for the C++, Rust, Go, and Zig standard
  libraries and your own types.
- **Disassembly** that names branch targets, including indirect ones resolved
  from the stopped state, and symbolization of code without debug information.
- **Signals** with gdb's default policies, changeable with `handle`.

uscope prefers saying what it cannot show to showing something wrong:
optimized-out values, unreadable memory, and unverifiable core dump modules
are reported as such.

## Language support

| Language | Values | Execution control |
| --- | --- | --- |
| C, C++, Rust (GCC, Clang, rustc) | Parameters, locals, and globals, including partly optimized-out values | Full |
| Zig 0.16 (LLVM backend) | Parameters, locals, and globals | Full; inline frames when emitted |
| Go 1.26 `gc` | Locals in `-N -l` builds; package globals in any build | Breakpoints and continue only: no source stepping, goroutines, or split-stack backtraces |

Thread-local storage is supported for glibc only.

## Quick start

uscope builds inside a pinned Nix environment:

```sh
just dev                    # enter the environment (or ./scripts/dev.sh)
just build-test-programs    # build the test programs
just run build/test-programs/basic
```

Then, at the `(uscope)` prompt:

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
uscope --attach PID                # attach; detaches on exit
uscope --core core.1234            # open a core dump
uscope --batch -e 'break f' -e run -e bt ./program
uscope dap                         # serve DAP on stdio
```

## Documentation

- [docs/cli.md](docs/cli.md): command-line flags and REPL commands.
- [docs/expressions.md](docs/expressions.md): the expression language.
- [docs/views.md](docs/views.md) and [docs/writing-views.md](docs/writing-views.md): views.
- [docs/dap.md](docs/dap.md): editor setup and DAP support.
- [AGENTS.md](AGENTS.md): architecture and development rules.

## Development

```sh
just          # format check, Clippy, and the test suite
just test X   # tests matching X
just stress   # the suite ten times under CPU load
just sim      # simulate random debugger sessions for 30 seconds
just all      # everything, before committing
```

See [AGENTS.md](AGENTS.md) for how the code is organized and tested.

## License

MIT or Apache-2.0, at your option.
