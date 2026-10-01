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

# attach to a running process; uscope discovers its executable through /proc
just dev --command cargo run -- --attach PID

# open a post-mortem core dump; uscope uses the executable recorded in the dump
just dev --command cargo run -- --core CORE
```

Use `--attach PID` or `-p PID` to attach to an existing process. `uscope` reads the
running executable through `/proc/PID/exe`, stops every native thread, and detaches
without terminating the process when the debugger exits. If automatic executable
discovery is unavailable, pass its path as the positional `EXECUTABLE` argument
alongside `--attach`.

Use `--core CORE` to open an ELF core dump written by the Linux kernel or by gdb's `gcore`. The dump is presented as one permanent stop at the thread that triggered it, with the terminating signal, its `si_code`, and the faulting address or sending process. Backtraces, registers, variables, globals, TLS, memory reads, and thread selection work as they do at a live stop; execution control, memory writes, and breakpoints fail explicitly. `info core` lists the process, signal, and every recorded module. Compressed dumps from `systemd-coredump` must first be extracted with `coredumpctl dump -o FILE`.

Each executable and shared library recorded by the dump is matched to its file on disk before use: by GNU build-id when the dump saved the note, and otherwise by comparing every saved byte of the file's read-only segments. Memory the dump did not save, such as unmodified code and read-only data, is read only from a proven file. A file that differs from the dump, or that nothing saved can verify, is a hard error. Pass `--allow-module-mismatch` to use its debug metadata anyway; its contents still never stand in for unsaved memory, and every such module is reported as a warning. A module whose file no longer exists is reported and its frames and unsaved memory stay unavailable. If the executable has moved, pass its path as the positional `EXECUTABLE` argument alongside `--core`.

At a breakpoint, use `registers` or `regs` to print the stopped thread's general register set.
Use `x <runtime-address> [byte-count]` to display a bounded target-memory range as hexadecimal bytes and printable ASCII. The default is 64 bytes and the CLI accepts at most 8192 bytes per command. Reads return the readable contiguous prefix and identify the first inaccessible address instead of discarding bytes read before a mapping boundary.
Use `print <name>` or `p <name>` to print one visible scalar. Lookup is local-first and then considers globals; exact namespace, module, container, linkage, and source-file qualifications are accepted. `print` with no argument continues to list only parameters followed by locals in the selected logical frame, including an inline function frame.
Use `globals [filter]` to list a bounded page of immutable global metadata without reading every value.
Scalar inspection supports one-piece values in memory, general-purpose and XMM registers, constants, computed DWARF stack values, and glibc TLS. Entry values, composite locations, non-default address spaces, and general cross-DIE expression evaluation remain explicitly unavailable.
Every inspected value has an explicit state: available, unavailable for a typed reason, readable but invalid for its source type, or backed by malformed debug metadata. Optimized-out values distinguish a missing location, an empty location, and explicitly undefined DWARF pieces. Typed reads across inaccessible memory remain all-or-unavailable and report the requested bytes, readable prefix length, and first inaccessible address; debugger operational failures remain request errors rather than convincing per-variable results.

| Language/compiler | Variable inspection | Execution control |
| --- | --- | --- |
| C, C++, Rust | Scalar parameters, locals, and qualified globals, including optimized partial availability | Breakpoints, stepping, inline frames, backtraces, and native threads |
| Zig 0.16 LLVM backend | Scalar parameters, locals, and qualified globals in Debug and ReleaseFast builds; PIE and non-PIE | Breakpoints, stepping, inline frames when emitted, backtraces, and native threads |
| Go 1.26 `gc` | Scalar parameters and locals in a `-N -l` build, plus package globals in unoptimized and optimized builds, at an explicit user breakpoint | Launch and continue only; source stepping, goroutine control, split-stack backtraces, and runtime-aware composite rendering are not supported |

Globals are module-aware. The runtime registry synchronizes executable shared-object mappings at coherent all-stop snapshots, publishes module load/unload events, rejects stale module identities, and relocates each value through its owning mapping. TLS lookup uses glibc's `libthread_db` for the selected native thread and supports the main executable and dynamically allocated DSO TLS. glibc is currently the only supported libc for TLS; an unavailable or incompatible provider is reported explicitly.
Backtraces unwind through every loaded module using its own call-frame information, so stops inside libc or another shared library still reach their callers. Frames are symbolized from the owning module's debug information; frames in modules without it show only their address.
Breakpoint stops automatically print three surrounding source lines on each side when source is available.
Use `list` or `l` to print that source context again for the current stop.
Use `stepi`, `step`, `next`, and `finish` for instruction and source-level execution control.

Use `watch <value-path>` to stop whenever a thread writes the memory a value occupies, and `awatch` to also stop on reads. `watch <address>:<byte-count>` watches explicit bytes. A watchpoint reports the access after the accessing instruction, with the value last observed by the debugger and the value once every thread stopped; a store of an identical value, or a failed `lock cmpxchg`, is still reported and marked unchanged. One instruction touching several watchpoints reports all of them, and every thread, including threads created later, is armed. Use `watchpoints` or `info watchpoints` to list them and `unwatch <id|all>` to delete them.

Watchpoints use the four x86-64 debug registers of every thread. Each register covers one naturally aligned 1, 2, 4, or 8 byte span, so a misaligned or larger value uses several, and perf hardware breakpoints held by the process can leave fewer; a watchpoint that does not fit is refused without arming anything. x86-64 cannot report reads alone, so `rwatch` is refused. Hardware only sees accesses made by user-mode instructions: the kernel filling a watched buffer in `read(2)` is never reported, which is why the reported old value is the last value the debugger observed, while vDSO code is reported.

A watchpoint resolved from an expression keeps watching the address it resolved, even after a pointer in the expression changes. Its lifetime follows the storage it names: static storage is watched until its module unloads, thread-local storage until its thread exits, and a local or parameter until its activation returns, a tail call replaces it, `longjmp` skips it, or execution leaves its lexical block. When that happens the watchpoint is removed and reported instead of describing reused memory; storage reached through a pointer or given as an address is never invalidated. Values held in registers or computed by the compiler, bit-fields, constants, and Go stack objects, which the runtime may move, cannot be watched. Watchpoints belong to one process: they are discarded when it exits or execs, disarmed before detaching, and debug registers left armed by an earlier tracer are cleared on attach so they cannot kill the process later.
Use `threads` to list stopped threads and `thread <id>` to select the thread used by register, variable, source, and backtrace commands.
Press Ctrl-C while the inferior is running to pause it at a coherent all-stop snapshot.
The interactive debugger is a plain terminal REPL, so output remains available in normal terminal scrollback. Submit an empty line to repeat the last command entered in the current interactive session. Use `--batch` with command files, `--eval`, or stdin when no interactive prompt is wanted.
Interactive output uses a restrained terminal-aware color palette while leaving source code text unstyled. Color is disabled for redirected output, `TERM=dumb`, `NO_COLOR`, and automatic batch output. Use `--color always` or `--color never` to override detection; `CLICOLOR` and `CLICOLOR_FORCE` are also honored.
