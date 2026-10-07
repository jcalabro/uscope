# Command line

```text
uscope [OPTIONS] EXECUTABLE [-- ARGS...]     launch a program
uscope [OPTIONS] --attach PID [EXECUTABLE]   attach to a process
uscope [OPTIONS] --core CORE [EXECUTABLE]    open a core dump
uscope views check|explain|replay ...        check views without a process
uscope dap [--port PORT | --listen ADDRESS] [--log FILE]
```

With `--attach` or `--core`, the executable is found through `/proc` or in the
dump; pass `EXECUTABLE` only when that fails. `uscope dap` is described in
[dap.md](dap.md), and `uscope views` in [views.md](views.md).

## Flags

| Flag | |
| --- | --- |
| `-p, --attach PID` | Attach to a running process. It is detached, still running, when uscope exits. |
| `--core CORE` | Open a core dump. |
| `--cwd DIR` | Working directory of a launched program. |
| `--env NAME=VALUE` | Set an environment variable of a launched program. Repeatable. |
| `--sysroot DIR` | Look up a core dump's files under `DIR`, a copy of the dumping machine's files. |
| `--module-path DIR` | Search `DIR` for a core dump's files missing or different at their recorded paths, by name and then by build-id. Repeatable. |
| `--allow-module-mismatch` | Use a core dump's module files that cannot be proven to match it. |
| `--source-map FROM TO` | Read sources recorded under `FROM` from `TO`. Repeatable; the first matching rule wins. |
| `--views FILE` | Load views from `FILE` ahead of the others. Repeatable; later files come first. |
| `-c, --command FILE` | Run the commands in `FILE`. Repeatable. |
| `-e, --eval COMMAND` | Run one command, after any `-c` files. Repeatable. |
| `--batch` | Exit after the commands instead of starting the REPL; with no `-c` or `-e`, read commands from stdin. A failing command ends the session with an error naming its source. |
| `--color auto\|always\|never` | Color output. `auto` honors `NO_COLOR`, `CLICOLOR`, `CLICOLOR_FORCE`, and `TERM`, and is off when output is not a terminal. |
| `--disassembly-syntax intel\|att` | Syntax for `disassemble`. Default `intel`. |

Launched programs run without address randomization, as under gdb, so
addresses are the same on every run.

## Commands

An empty line repeats the last `continue`, stepping, `up`, `down`, `x`, or
`list` command. Lines starting with `#` are ignored. `help [command]` describes
each command.

### Running

| Command | |
| --- | --- |
| `run`, `r` | Launch the program. |
| `continue`, `c` | Resume every thread. |
| `step`, `s` / `next`, `n` | Step into / over calls, by source line. |
| `step task` | Step into the task the line starts, such as a goroutine. |
| `stepi`, `si` / `nexti`, `ni` | Step one instruction, into / over calls. |
| `finish`, `fin` | Run until the selected frame returns. |
| `quit`, `q` | Exit, killing a launched program and detaching from an attached one. |

Ctrl-C pauses a running program. The terminal's `SIGINT` also reaches the
program, so resuming discards a pending `SIGINT`, as gdb does.

The debugger is all-stop: every thread stops together and resumes together.
While one thread steps, the others run, so stepping over a call that waits
for another thread, such as `pthread_join`, completes. Another thread's
breakpoint, watchpoint, or signal ends the step where it happened. A signal
that arrives during a step runs its handler at full speed, and the step
continues when the handler returns.

`finish` runs until the selected frame returns to its caller, so a recursive
call that returns to the same address from a deeper activation keeps running.
It supports frames of the main executable and inline frames of the innermost
activation. Stepping always starts from the innermost frame. When `finish`
stops as the function returns, it shows what the function returned, as
`returned (int) count = 42`, read where the function's calling convention
leaves each result, so an optimized function's results show as well; `print`
lists them with the frame's variables until the program runs again. Go's
register ABI is the convention uscope knows; a function of another language
shows nothing returned.

In a program whose language runtime schedules tasks, such as Go's
goroutines, a step belongs to the task it began in. It follows the task to
whichever thread the runtime resumes it on, other tasks that run the same
code meanwhile never end it, and its frames are followed when the runtime
moves the task's stack. `step task`, or the runtime's own name for a task
such as `step goroutine`, steps over the line, unless its task starts
another task meanwhile, as a `go` statement does, even in a function the
line calls: the step then belongs to the first task started, and stops
where that task's function begins, through the wrapper that passes it its
arguments. The started task is then selected. A line that starts no task
ends as `next` does.

`step` stops only in code the program's author wrote: it passes through
the runtime's private machinery, compiler-generated wrappers, and stack
switches to the code they call, and steps out of them where they call none.
A step begun in the runtime may stop there. `step` at a `return` enters the
deferred calls it runs; `next` and `finish` run them, but stop in a deferred
call that a panic runs. The body of a loop over an iterator function is a
function the iterator calls, which steps treat as the loop's own code:
`next` enters the body from the loop's line, goes from one pass of the body
to the next and on past the loop, and `finish` in the body runs the rest of
the loop. None of them stops in the iterator. In a Go program that calls C,
steps go between Go and C as between functions of one language, through
cgo's code and the runtime's: `step` at a call enters the function called,
and `finish` in Go that C called stops in the C.

A forked child is not followed: it runs on its own, without the breakpoints
it inherited. A program that calls `exec` is followed, with its breakpoints.

### Breakpoints

| Command | |
| --- | --- |
| `break`, `b` *location* [*hit-condition*] | Break at a `function`, `file:line`, `file:function`, or `0xaddress`. |
| `breakpoints`, `info breakpoints` | List breakpoints and their hit counts. |
| `delete`, `d` *id*\|`all` | Delete breakpoints. |
| `condition` *id* [*expression*] | Stop only where the [expression](expressions.md) is true; with none, always. |
| `hits` *id* *hit-condition*\|`always` | Replace the hit condition, keeping the count. |
| `ignore` *id* *count* | Skip the next *count* hits. |

`condition`, `hits`, and `ignore` change a watchpoint too, named by `w` and
its id: `condition w2 counter > 10`, `hits w2 %100`.

Addresses are always `0x`-prefixed, so `break add` names a function. Functions
without debug information, such as libc's, break at their symbol, and
breakpoints in a shared library wait until it loads.

A function is first looked up by its whole name, and every function with
that name and code gets a location, inlined copies included. Go functions
can also be named as Go source names them:

- by package name or import path: `http.(*Server).Serve` or
  `net/http.(*Server).Serve`;
- by the method's receiver, with or without its `*`: `(*T).M`, `T.M`, and
  `pkg.T.M` all name `T`'s method `M`, whichever receiver it declares, as in
  Delve, since a type has at most one method of a name;
- by a generic function's name, which names every instantiation:
  `main.Sum` breaks in `main.Sum[go.shape.int]` and the rest;
- by a closure's compiler name, `main.main.func1`;
- unqualified, `Serve` or `(*Server).Serve`. At a stop this first means the
  selected frame's package, and the breakpoint keeps the qualified name,
  such as `main.Serve`.

A name that matches functions of more than one package, or more than one
function of a package, is refused with each candidate's qualified name, so
`break main` asks for `main.main` or `runtime.main`. Wrappers the compiler
generates, such as ABI wrappers, are never chosen. A Go `file:line` with no
statement is refused with the nearest lines that have one; in other
languages, as in gdb, it moves to the next line with code in its function.

A breakpoint is a trap byte written into the program's code, so it moves
wherever that code moves. When a library's code or the vDSO moves, as under a
checkpoint restore, a function breakpoint follows it, even when the program
runs the moved code at once. A location whose memory was unmapped or
overwritten is dropped, so an address breakpoint there lists 0 locations.
Nothing reports a move or copy of code no file backs, such as a JIT's, so a
trap carried with that code stops the program as a `SIGTRAP`.

A hit condition is an operator and a count: `==3` stops at the third hit only,
`>=5` at the fifth and later, `%10` at every tenth, and `!=`, `<`, `<=`, and `>`
work likewise. A bare count is refused, because debuggers disagree about what
it means. Every time a thread reaches one of a breakpoint's locations is a hit,
whether or not it stops, as in gdb, and counts restart with each new process.
Hits that do not stop are invisible: the thread steps over the trap while the
others stay stopped, then everything resumes, including a `next` or `finish`
in progress.

Breakpoints and watchpoints can be changed while the program runs: every
thread stops briefly for the change and resumes without a reported stop.

### Watchpoints

| Command | |
| --- | --- |
| `watch` *target* [`if` *condition*] | Stop when a store changes the value. |
| `watch -w` *target* [`if` *condition*] | Stop at every store, even of the same value. |
| `awatch` *target* [`if` *condition*] | Stop at every load or store. |
| `watchpoints`, `info watchpoints` | List watchpoints and their hit counts. |
| `unwatch` *id*\|`all` | Delete watchpoints. |

The *target* is an expression, or `0xaddress:byte-count` for raw bytes. A stop
reports the access after the instruction that made it, with the value last
observed and the current value. One instruction that touches several
watchpoints reports all of them.

Watchpoints use the four x86-64 debug registers of every thread. Each covers
an aligned 1, 2, 4, or 8 bytes, so a misaligned or large value needs several,
and a watchpoint that does not fit is refused. x86-64 cannot trap reads alone,
so `rwatch` is refused. Only user-mode accesses trap: a watched buffer filled
by `read(2)` is not reported, which is why the old value is the last one the
debugger observed.

`watch` traps every store and compares the bytes with those last observed;
when they are equal, the storing thread resumes at once without stopping the
others. A debugger write with `set` counts as observed.

Watchpoints take conditions and hit conditions as breakpoints do, as in
`watch counter if counter % 100 == 0`. Every access the watchpoint reports
is a hit: every store or access, or for `watch`, every store that changes
the value. The condition is evaluated in the accessing thread's innermost
frame, after the access, so it sees the new value; a condition naming
another function's locals fails there, and a condition that cannot be
evaluated stops and says why. A hit that does not stop is invisible, like
a breakpoint's, and what it stored becomes the value last observed, so the
next stop reports the change from it, as gdb does.

A watchpoint on an expression keeps watching the address it first resolved to.
It ends with its storage, and the end is reported: a static when its module
unloads, thread-local storage when its thread exits, and a local when its
frame returns or its block is left. A goroutine's local belongs to its
goroutine, and moves with it: when the runtime copies the goroutine's stack
elsewhere to grow or shrink it, the watchpoint stops watching during the
copy and then watches the local at its new address, which `info watchpoints`
shows. A copy the debugger cannot follow ends the watchpoint and says so.
Register values, bit-fields, and constants cannot be watched. Watchpoints
are discarded when the process exits or execs, and cleared before
detaching.

### Stack and frames

| Command | |
| --- | --- |
| `backtrace`, `bt` | Show the selected thread's or goroutine's stack. |
| `frame`, `fr` [*level*] | Show the selected frame, or select one by level. |
| `up` / `down` [*count*] | Select a caller / callee frame. |
| `where` | Show the selected frame's location and module. |
| `list`, `l` | Show source around the selected frame's line. |
| `registers`, `regs` | Show the selected frame's general registers. |

Breakpoint, step, and watchpoint stops print three source lines on each side.
Backtraces unwind through every loaded module using its own call-frame
information. Frames without debug information are named `symbol+offset` from
the module's ELF symbol tables, including MiniDebugInfo; code no symbol covers
is `<unknown>` rather than borrowing a neighbor's name. Rust and C++ symbols
are demangled. The vDSO, the code the kernel maps into every process for
calls such as `clock_gettime`, is the module `[vdso]`; no file backs it, so
it is read from the process's memory.

A Go thread runs the runtime's code on a stack of its own, and signal
handlers on another, and a backtrace follows the runtime from them onto the
goroutine's stack. When a backtrace crosses stacks, each run of frames is
headed by whose stack it is on: the task's (the goroutine's), the runtime's,
the signal stack, or the thread's. The runtime's own functions are dimmed.
Go's calls into C run the C on the runtime's stack, and C's calls back into
Go run the Go on the goroutine's, so a backtrace from either shows the
frames of both languages between them.
The body of a Go `range` over a function is a function of its own, named
like `main.counted-range1`, which the iterator calls; the iterator's frames
between the body and its loop's function say so, as `(the iterator of #2's
loop)`. The body's frame shows the variables of the loop's function it
uses, and the function's frame shows the rest.

The selected frame applies to `print`, `watch`, `where`, `list`,
`disassemble`, `registers`, and `finish`. Each stop selects the innermost
frame; each thread keeps its own selection until the next stop. An outer
frame's variables are shown as they were at its call, read from where its
callees saved them. A value in a register that callees may overwrite without
saving is reported as not saved rather than shown with the callee's value.
Past Go code, only the stack pointer is recovered.

### Values

| Command | |
| --- | --- |
| `print`, `p` [*expression*] | Print a value, or every parameter and local of the selected frame. `/x` prints integers in hexadecimal and `/r` without views. |
| `set` [`var`] *assignment* | Assign, as in `set var x = y + 1`. |
| `whatis` *expression* | Show an expression's type. |
| `ptype` *expression or type* | Show a type's definition. |
| `globals` [*filter*] | List globals and their types without reading them. |
| `info view` *expression* | Explain which view presents a value. |
| `set views on`\|`off` | Present values through views, or as stored. |
| `views` [`load` *file*\|`clear`\|`check`\|`explain` *type*\|`record` *file* *expression*] | Manage view files; see [views.md](views.md). |

Expressions are described in [expressions.md](expressions.md). Every value has
an explicit state: available, unavailable for a stated reason (optimized out,
unreadable memory), or invalid for its type. Reads across unreadable memory
report the address that failed. A goroutine's frame holding a pointer below
its own stack pointer, into memory only its callees use, holds a stale
pointer, as a slot Go left unadjusted when it moved the stack does; what it
points at is unavailable rather than shown from whatever is there now.

### Memory, symbols, and disassembly

| Command | |
| --- | --- |
| `x` *0xaddress* [*bytes*] | Dump memory as hexadecimal and ASCII; 64 bytes by default, at most 8192. A read that crosses into unmapped memory shows what it read and where it stopped. |
| `disassemble`, `disas` [*function*\|*0xaddress*] [*count*] | Disassemble a whole function, or *count* instructions from an address. |
| `address` *symbol* | Show a symbol's runtime address. |
| `info symbol` *0xaddress* | Name the module, section, and symbol containing an address. |

`disassemble` shows every range of a function, including split `.cold` parts,
with breakpoint traps hidden. Each line shows the address, its symbol offset,
the bytes, and the instruction; source lines are announced as they change.
Direct branch targets and PC-relative operands are named. An indirect jump,
call, or return names its target as the stopped state determines it, read
from memory such as a GOT slot and, for the instruction about to execute, from
registers. Anything that cannot be determined, such as a slot a core dump did
not save, is reported rather than guessed. Instructions are decoded only
forward from known instruction starts, and overlaps are reported.

`info symbol` names data only within a symbol's declared size, never the
nearest preceding symbol.

### Threads and signals

| Command | |
| --- | --- |
| `threads` | List threads, with the goroutine each runs. |
| `thread` *id* | Select a thread. |
| `tasks` [`-a`] [`-g`] [`-t`] | List a runtime's tasks, Go's goroutines: `-a` with the runtime's own, `-g` grouped by place, `-t` each with its stack. |
| `task` [*id* [*command*]] | Show the selected task, select one, or run an inspecting command in one. |
| `handle` *signal* [`stop`\|`nostop`] [`print`\|`noprint`] [`pass`\|`nopass`] | Change how a signal is handled. `stop` implies `print`, and `noprint` implies `nostop`. |
| `info signals` | List every signal's policy. |

Each runtime's own name for its tasks names these commands too:
`goroutines` and `goroutine` in Go. A goroutine is listed where the code the
program wrote has it, past the
runtime's machinery, as the runtime's own goroutine dump shows it: a worker
waiting on a channel is at its receive, not in `runtime.gopark`. Each line
gives the goroutine's id, that place, what it does in the runtime's words,
such as `chan receive`, its profiler labels, as in `{job: resize}`, and the
thread it is on. A goroutine of only the runtime's code is named by the
function it began in. A core dump's goroutines are listed as a live
program's are. A Go library that a C program hosts carries a runtime of
its own, whose goroutines are listed once it loads; a thread of the host's
that calls into Go runs a goroutine for the call. A Go program built
without debug information (`-ldflags=-w`, or `-s -w`) still has its frames
named and unwound by Go's own function table, and its function and line
breakpoints and steps work by it, but its goroutines cannot be read, which
`goroutines` and `$task` say. Selecting a goroutine,
parked or running, points `backtrace`, `frame`, `print`, `registers`, and
the other inspecting commands at it, and `$task` in an expression is its id.
`goroutine` *id* *command* runs one of those commands in the goroutine and
then selects again what was selected.

Signals follow gdb's defaults. `SIGALRM`, `SIGURG`, `SIGCHLD`, `SIGWINCH`,
`SIGPROF`, `SIGVTALRM`, `SIGIO`, and `SIGPWR` are delivered without stopping;
`SIGINT` stops and is discarded; every other signal stops and is delivered on
resume. Go preempts goroutines with `SIGURG`; one that arrives while a thread
steps, steps over a breakpoint, or runs without the others waits until the
thread continues with them, since its handler could wait for the stopped
threads.

A language runtime that handles signals itself changes their defaults:
`SIGSEGV`, `SIGBUS`, and `SIGFPE` are delivered silently to a Go program,
whose runtime turns a fault into a panic. A `handle` command still applies
over that. What the runtime then reports stops instead:

- a panic nothing recovered, as it ends the program;
- a fatal error, such as `all goroutines are asleep - deadlock!`, or a
  fault the runtime cannot turn into a panic, as one in C is.

The stop prints the message the runtime prints, chained panics and all, and
selects the frame that panicked or faulted, below the runtime's own. A
breakpoint instruction of the program's own, such as Go's
`runtime.Breakpoint()`, stops too, and the program goes on past it.

## Core dumps

`--core` opens a dump written by the kernel or gdb's `gcore` as one permanent
stop at the thread that triggered it. Backtraces, frames, values, memory, and
threads work as at a live stop; running, writing, and breakpoints fail.
`info core` shows the process, the signal, and every module. Extract a
`systemd-coredump` dump with `coredumpctl dump -o FILE` first.

Each executable and library in the dump is matched to a file on disk by its
build-id or, without one, by comparing every byte the dump saved of its
read-only segments. Memory the dump did not save, such as code, is read only
from a matching file. A file that does not match is an error unless
`--allow-module-mismatch` is given, and even then it is used only for debug
information. A missing file is reported with its build-id, and its frames
and unsaved memory are unavailable. The vDSO is read from the dump itself,
and is reported missing if the dump did not save it.

For a dump from another machine or a container, `--sysroot DIR` resolves
every recorded path inside `DIR` as if it were `/`, and `--module-path DIR`
finds renamed or relocated copies, used only when they match.

## Sources

Source files are read from the paths in the debug information, and read
lazily. For a program built elsewhere, `--source-map FROM TO` reads files
recorded under `FROM` from `TO`, matching whole path components. A missing
source names every path tried. Breakpoints still use recorded paths, or their
trailing components, as in `break main.c:10`.

## Output

Interactive output is colored when stdout is a terminal, leaving source text
plain. History is kept in `$XDG_STATE_HOME/uscope/history`. Development builds
record a flight recording of every request and ptrace call under
`target/flight-recorder`; `USCOPE_FLIGHT_RECORDING=PATH` chooses the file,
and an empty value turns it off.
