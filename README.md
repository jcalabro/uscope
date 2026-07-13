# uscope

`uscope` is a Linux x86-64 native debugger written in Rust.

## Development

Enter the pinned development environment and run the checks:

```sh
# either of these:
./scripts/dev.sh
just dev

# then, build and run the test binaries
just build-test-programs

# run the linter and all tests
just

# start the debugger
just run build/test-programs/basic
```

At a breakpoint, use `registers` or `regs` to print the stopped thread's general register set.
Use `print <name>` or `p <name>` to print one visible scalar, or `print` with no argument to list parameters followed by local variables in the selected logical frame, including an inline function frame.
Scalar inspection supports one-piece values in memory, general-purpose and XMM registers, constants, and computed DWARF stack values. Entry values, composite locations, non-default address spaces, TLS, and cross-DIE evaluation remain explicitly unavailable.

| Language/compiler | Variable inspection | Execution control |
| --- | --- | --- |
| C, C++, Rust | Scalar parameters and locals, including optimized partial availability | Breakpoints, stepping, inline frames, backtraces, and native threads |
| Zig 0.16 LLVM backend | Scalar parameters and locals in Debug and ReleaseFast builds; PIE and non-PIE | Breakpoints, stepping, inline frames when emitted, backtraces, and native threads |
| Go 1.26 `gc` | Scalar parameters and locals in a `-N -l` build at an explicit user breakpoint | Launch and continue only; source stepping, goroutine control, split-stack backtraces, and runtime-aware composite rendering are not supported |

Package-level and file-level globals are not yet part of `print`; their implementation is the next planned phase.
Breakpoint stops automatically print three surrounding source lines on each side when source is available.
Use `list` or `l` to print that source context again for the current stop.
Use `stepi`, `step`, `next`, and `finish` for instruction and source-level execution control.
Use `threads` to list stopped threads and `thread <id>` to select the thread used by register, variable, source, and backtrace commands.
Press Ctrl-C while the inferior is running to pause it at a coherent all-stop snapshot.
The interactive debugger is a plain terminal REPL, so output remains available in normal terminal scrollback. Submit an empty line to repeat the last command entered in the current interactive session. Use `--batch` with command files, `--eval`, or stdin when no interactive prompt is wanted.
Interactive output uses a restrained terminal-aware color palette while leaving source code text unstyled. Color is disabled for redirected output, `TERM=dumb`, `NO_COLOR`, and automatic batch output. Use `--color always` or `--color never` to override detection; `CLICOLOR` and `CLICOLOR_FORCE` are also honored.
