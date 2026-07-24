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
Use `x <runtime-address> [byte-count]` to display a bounded target-memory range as hexadecimal bytes and printable ASCII. The default is 64 bytes and the CLI accepts at most 8192 bytes per command. Reads return the readable contiguous prefix and identify the first inaccessible address instead of discarding bytes read before a mapping boundary.
Use `print <name>` or `p <name>` to print one visible scalar. Lookup is local-first and then considers globals; exact namespace, module, container, linkage, and source-file qualifications are accepted. `print` with no argument continues to list only parameters followed by locals in the selected logical frame, including an inline function frame.
Use `globals [filter]` to list a bounded page of immutable global metadata without reading every value.
Scalar inspection supports one-piece values in memory, general-purpose and XMM registers, constants, computed DWARF stack values, and glibc TLS. Entry values, composite locations, non-default address spaces, and general cross-DIE expression evaluation remain explicitly unavailable.

| Language/compiler | Variable inspection | Execution control |
| --- | --- | --- |
| C, C++, Rust | Scalar parameters, locals, and qualified globals, including optimized partial availability | Breakpoints, stepping, inline frames, backtraces, and native threads |
| Zig 0.16 LLVM backend | Scalar parameters, locals, and qualified globals in Debug and ReleaseFast builds; PIE and non-PIE | Breakpoints, stepping, inline frames when emitted, backtraces, and native threads |
| Go 1.26 `gc` | Scalar parameters and locals in a `-N -l` build, plus package globals in unoptimized and optimized builds, at an explicit user breakpoint | Launch and continue only; source stepping, goroutine control, split-stack backtraces, and runtime-aware composite rendering are not supported |

Globals are module-aware. The runtime registry synchronizes executable shared-object mappings at coherent all-stop snapshots, publishes module load/unload events, rejects stale module identities, and relocates each value through its owning mapping. TLS lookup uses glibc's `libthread_db` for the selected native thread and supports the main executable and dynamically allocated DSO TLS. glibc is currently the only supported libc for TLS; an unavailable or incompatible provider is reported explicitly.
Breakpoint stops automatically print three surrounding source lines on each side when source is available.
Use `list` or `l` to print that source context again for the current stop.
Use `stepi`, `step`, `next`, and `finish` for instruction and source-level execution control.
Use `threads` to list stopped threads and `thread <id>` to select the thread used by register, variable, source, and backtrace commands.
Press Ctrl-C while the inferior is running to pause it at a coherent all-stop snapshot.
The interactive debugger is a plain terminal REPL, so output remains available in normal terminal scrollback. Submit an empty line to repeat the last command entered in the current interactive session. Use `--batch` with command files, `--eval`, or stdin when no interactive prompt is wanted.
Interactive output uses a restrained terminal-aware color palette while leaving source code text unstyled. Color is disabled for redirected output, `TERM=dumb`, `NO_COLOR`, and automatic batch output. Use `--color always` or `--color never` to override detection; `CLICOLOR` and `CLICOLOR_FORCE` are also honored.
