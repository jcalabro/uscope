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
Use `print <name>` or `p <name>` to print one visible stack scalar, or `print` with no argument to list parameters followed by local variables in the selected logical frame, including an inline function frame.
Initial scalar inspection supports C, C++, and Rust debug information. Optimized register, computed, entry-value, and composite locations remain explicitly unavailable.
Breakpoint stops automatically print three surrounding source lines on each side when source is available.
Use `list` or `l` to print that source context again for the current stop.
Use `stepi`, `step`, `next`, and `finish` for instruction and source-level execution control.
Use `threads` to list stopped threads and `thread <id>` to select the thread used by register, variable, source, and backtrace commands.
Press Ctrl-C while the inferior is running to pause it at a coherent all-stop snapshot.
The interactive debugger is a plain terminal REPL, so output remains available in normal terminal scrollback. Submit an empty line to repeat the last command entered in the current interactive session. Use `--batch` with command files, `--eval`, or stdin when no interactive prompt is wanted.
