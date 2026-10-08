# Command line

```text
uscope [OPTIONS] EXECUTABLE [-- ARGS...]          launch a program
uscope [OPTIONS] --attach PID|NAME [EXECUTABLE]   attach to a process
uscope [OPTIONS] --core CORE [EXECUTABLE]         open a core dump
uscope [OPTIONS] --launch NAME [-- ARGS...]       start a launch configuration
uscope config path|show|check|init|trust|untrust|trusted
uscope views check|explain|replay ...             check views without a process
uscope dap [--port PORT | --listen ADDRESS] [--log FILE]
```

With `--attach` or `--core`, the executable is found through `/proc` or in the
dump; pass `EXECUTABLE` only when that fails. `--attach NAME` attaches to the
one process with that name, matched exactly as `pgrep -x` matches, and fails
naming them when there are several. `uscope` alone starts the project's launch
configuration when it has exactly one; see [Settings](#settings). `uscope dap`
is described in [dap.md](dap.md), and `uscope views` in [views.md](views.md).

## Flags

| Flag | |
| --- | --- |
| `-p, --attach PID\|NAME` | Attach to a running process, by id or by name. It is detached, still running, when uscope exits. |
| `-l, --launch NAME` | Start the project's launch configuration `NAME`. |
| `--core CORE` | Open a core dump. |
| `--cwd DIR` | Working directory of a launched program. |
| `--env NAME=VALUE` | Set an environment variable of a launched program. Repeatable. |
| `--sysroot DIR` | Look up a core dump's files under `DIR`, a copy of the dumping machine's files. |
| `--module-path DIR` | Search `DIR` for a core dump's files missing or different at their recorded paths, by name and then by build-id. Repeatable. |
| `--allow-module-mismatch` | Use a core dump's module files that cannot be proven to match it. |
| `--source-map FROM TO` | Read sources recorded under `FROM` from `TO`. Repeatable; the first matching rule wins. |
| `--views FILE` | Load views from `FILE` ahead of the others. Repeatable; later files come first. |
| `--debug-directory DIR` | Search `DIR` for the separate debug files of stripped modules, ahead of `[debug-info] directories`, `NIX_DEBUG_INFO_DIRS`, and `/usr/lib/debug`. Repeatable. |
| `--debuginfod` | Download debug files no directory holds from the servers `DEBUGINFOD_URLS` lists, as `[debug-info] debuginfod = true` does. |
| `-c, --command FILE` | Run the commands in `FILE`. Repeatable. |
| `-e, --eval COMMAND` | Run one command, after any `-c` files. Repeatable. |
| `--batch` | Exit after the commands instead of starting the REPL; with no `-c` or `-e`, read commands from stdin. A failing command ends the session with an error naming its source. |
| `--color auto\|always\|never` | Color output, over `[ui] color`. `auto` honors `NO_COLOR`, `CLICOLOR`, `CLICOLOR_FORCE`, and `TERM`, and is off when output is not a terminal. |
| `--disassembly-syntax intel\|att` | Syntax for `disassemble`, over `[disassembly] syntax`. Default `intel`. |
| `--config FILE` | Read `FILE` instead of the user's settings file, as `USCOPE_CONFIG=FILE` does. |
| `--no-config` | Read no settings files, as an empty `USCOPE_CONFIG` does. |
| `--trust-project` | Use the project's startup commands, aliases, and launch configurations in this session without asking. |

Launched programs run without address randomization, as under gdb, so
addresses are the same on every run.

## Settings

Settings are TOML, in three files, each optional:

1. The user's, `$XDG_CONFIG_HOME/uscope/config.toml` or
   `~/.config/uscope/config.toml`, for every project.
2. The project's, `.uscope/config.toml` at the project root, usually
   committed.
3. The project's local file, `.uscope/config.local.toml`, for one person;
   list it in the project's `.gitignore`, which uscope never edits.

The project root is the nearest directory, from the one uscope runs in
upwards, that holds `.uscope/`, then the nearest that holds `.git`, then the
directory itself. It is where views and saved breakpoints are found too, and
source paths inside it are shown relative to it.

A setting comes from the first of these that sets it: a flag, the
environment (`NO_COLOR`, `CLICOLOR_FORCE`, and `CLICOLOR` outrank every
file), the local file, the project's, the user's, and the default. Tables
merge key by key, and lists replace. Startup commands instead run from every
file in order, user, project, local, then the launch configuration's, then
`-c` files and `-e` commands; launch configurations merge by name, so the
local file can change one key of the project's.

`uscope config init` writes the user's file with every setting at its
default, commented out and described; `uscope config show` prints every
setting in effect and the file, flag, or variable it comes from; `uscope
config path` prints the files a session here would read. An unknown key, a
wrong type, or a value out of range stops startup with the file, line,
column, and nearest valid key, as in `config.toml:3:1: unknown key 'colour'
in [ui]; did you mean 'color'?`; `--no-config` starts anyway, and `uscope
config check` checks every file, for a project's CI.

| Table | Settings |
| --- | --- |
| `[ui]` | `color`, `theme` (`default` or `light`), `unicode`, `hyperlinks` (OSC 8 links on `file:line`), `paths` (`relative`, `absolute`, or `name`), `pager`, `editor`, `prompt`, `confirm-quit` |
| `[theme]` | One role's style over the theme's, such as `changed = "bold yellow"`: a color (`red`, `bright-red`, `0`-`255`, `#rrggbb`) with `bold`, `dim`, `italic`, `underline`, `reverse`, and `on COLOR` |
| `[source]` | `context = [before, after]` lines at stops and in `list`, `highlight`, `tab-width` |
| `[stop]` | `show`, the sections a stop prints, and their sizes |
| `[print]` | `style` (`compact` or `pretty`), `radix`, `width`, `indent`, `max-depth`, `max-elements` |
| `[disassembly]` | `syntax`, `show-bytes` |
| `[step]` | `runtime`: `skip` passes over a language runtime's own code, `enter` stops in it, as `set step-runtime` chooses |
| `[breakpoints]` | `save` |
| `[debug-info]` | `directories`, searched for separate debug files after `--debug-directory`'s and relative to the project root, and `debuginfod` |
| `[history]` | `size` |
| `[signals]` | `SIGUSR1 = "nostop noprint pass"`, as `handle` takes them |
| `[[source-map]]` | `from` and `to`, as `--source-map` takes them, after the command line's rules; `to` is relative to the project root |
| `[aliases]` | `bm = "break main"`: a word that stands for a command line; it cannot hide a command |
| `[startup]` | `commands`, run at startup |
| `[projects]` | `trust`, only in the user's file |
| `[[launch]]` | Launch configurations, below |

### Launch configurations

A project describes how its programs are debugged, as VS Code's
`launch.json` does:

```toml
[[launch]]
name = "server"
program = "build/server"          # relative to the project root, as every path here is
args = ["--port", "8080"]
env = { RUST_LOG = "debug" }
cwd = "."                         # the project root by default
startup = ["break handle_request", "run"]

[[launch]]
name = "attached"
attach = "server"                 # a process id, or a name

[[launch]]
name = "crash"
core = "core.server"              # with sysroot, module-path, and allow-module-mismatch
program = "build/server"
```

`uscope --launch server` starts one, and `uscope` alone starts the only one;
with several, it lists them and exits rather than choose. Arguments after
`--` replace a configuration's `args`, `--env` adds to and overrides its
`env`, and `--cwd` replaces its `cwd`. `views` names view files, as
`--views` does.

### Trusting a project

A project's files arrive with a clone, so its startup commands, aliases, and
launch configurations, which act, apply only once trusted; everything else
applies at once. The user's `[projects] trust` chooses how:

- `ask`, the default: a session in a terminal shows them and asks `yes`,
  `once`, or `no`. `yes` records them in `$XDG_STATE_HOME/uscope/trust.toml`,
  readable, and the session asks again only when they change. A session that
  cannot ask, such as `--batch`, fails and names `--trust-project` and
  `uscope config trust`, which trust them for the session and until they
  change. `uscope config untrust` forgets a project, and `uscope config
  trusted` lists those trusted.
- `always`: every project's files apply, as running its `Makefile` would.
- `never`: they never apply, and the session says what it left out.

## Commands

An empty line repeats the last `continue`, stepping, `up`, `down`, `x`, or
`list` command. Lines starting with `#` are ignored. `help [command]` describes
each command, and lists the aliases the settings define. A prefix that begins
one command's name runs it, as `disp` does `display`; one that begins several
is answered with them, and an alias the settings define wins over a prefix. A
mistyped command is answered with the nearest ones.

Tab completes the word at the cursor: a command, a location after `break`,
`tbreak`, `advance`, `jump`, or `disassemble` (a function, or a file followed by its
line or function), breakpoint and watchpoint ids, `info` and `handle` words,
and in expressions the selected frame's variables, globals, and, after `.` or
`->`, the members of the value before it, which the debugger reads.

At a terminal, output taller than the screen goes through `[ui] pager`: by
default `$PAGER`, or `less -FRX`; `never` turns it off. `quit` asks before
killing a launched program that is still alive, unless `[ui] confirm-quit` is
false; end-of-input and scripts never ask. `edit` opens the selected frame's
line in `[ui] editor`, a command in which `{path}` and `{line}` are replaced,
or `$VISUAL` or `$EDITOR` with `+line path`.

### Running

| Command | |
| --- | --- |
| `run`, `r` | Launch the program. |
| `continue`, `c` | Resume every thread. |
| `step`, `s` / `next`, `n` | Step into / over calls, by source line. |
| `step task` | Step into the task the line starts, such as a goroutine. |
| `step` *function* \| `*`*0xaddress* | Step into one call of the line: the first that calls *function*, or the call instruction at an address. |
| `info calls` | List the calls of the selected thread's line that `step` can go into. |
| `stepi`, `si` / `nexti`, `ni` | Step one instruction, into / over calls. |
| `set step-runtime on`\|`off` | Let steps stop in a language runtime's own code, or pass over it. |
| `finish`, `fin` | Run until the selected frame returns. |
| `advance`, `adv` *location* | Run until the selected thread reaches a location, or the selected frame returns. |
| `jump`, `j` *location* | Move the selected thread, without running it, to resume at a location in its function. |
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
register ABI and the System V convention of C and C++ are the conventions
uscope knows: a C or C++ value is shown named for its function, from the
registers it was returned in, from `st0` for a `long double`, or from the
memory whose address the function returned for a larger one. A small C++
class's place depends on whether copying it is trivial, which Clang records
and GCC does not, so with GCC such a value is shown as unknown for that
reason. Rust and Zig leave their own conventions unspecified, so uscope
shows their scalars, which they return as C does, and their aggregates as
unknown; Zig's LLVM backend describes a function that returns a struct as
returning nothing, so nothing is shown.

`step` *function* steps into one call of a line that makes several, such as
`step add` on `add(twice(x), inc(x))`. The line's other calls run to their
returns, as `next` runs a call, and the step stops where the chosen
function's source begins. A call whose function has no source is stepped
through, as `step` would, and a line that ends before reaching the call
ends the step as `step` does. `info calls` lists the line's calls from the
stopped instruction on, in address order: each call's address and the
function it calls, or `(indirect)` for a call through a pointer, which
`step *`*0xaddress* names.

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

In an async function, a step follows the function's future across its
awaits. `next` over an await that is not ready lets the task go on, waits
for that future to be polled again, on whichever thread, and stops at the
function's next line; other tasks running the same function meanwhile pass.
`finish` and `advance` wait the same way, until the function returns to its
awaiter, where `finish` shows what it returned. A step of a task no thread
runs, selected with `task N`, waits for the task to resume. A breakpoint, a
signal, or `pause` ends a waiting step as it ends any. When the future goes
away meanwhile, the step says how: `stopped as task 7 was cancelled` as its
runtime cancels the task, `stopped as task 7 finished` as the task's own
future returns, and `whose future was dropped` on the next line of the code
that dropped it, as `select!` and timeouts do. Where an optimized build
inlines an async function into its awaiter and keeps no record of its
future, `finish` from it goes on to the awaiter's next line, since its
return and its waiting look alike there.

`step` stops only in code the program's author wrote: it passes through
the runtime's private machinery, compiler-generated wrappers, and stack
switches to the code they call, and steps out of them where they call none.
A step begun in the runtime may stop there, and `set step-runtime on`, or
`runtime = "enter"` in `[step]`, lets every step stop there, as in tokio's
`recv` on `step` into `rx.recv().await`; `set step-runtime off` restores
the default. Backtraces fold the runtime's frames either way. `step` at a `return` enters the
deferred calls it runs; `next` and `finish` run them, but stop in a deferred
call that a panic runs. The body of a loop over an iterator function is a
function the iterator calls, which steps treat as the loop's own code:
`next` enters the body from the loop's line, goes from one pass of the body
to the next and on past the loop, and `finish` in the body runs the rest of
the loop. None of them stops in the iterator. In a Go program that calls C,
steps go between Go and C as between functions of one language, through
cgo's code and the runtime's: `step` at a call enters the function called,
and `finish` in Go that C called stops in the C.

`advance` runs until the selected thread reaches any location the *location*
names, as a breakpoint there would stop it, or until the selected frame
returns, whichever comes first, so `advance 42` leaves a loop without
leaving the function. Other threads run meanwhile and pass the location
without stopping. A breakpoint reached on the way stops it as usual.

`jump` moves the selected thread to resume at a line of the selected
frame's file, such as `jump 42`, `jump +2`, or `jump -3`, or at a
`file:line` or `0xaddress`, without running anything. The location must be
in the code of the function the thread is stopped in, at one place: leaving
the function would leave its frame for another's, so a location elsewhere,
or a line inlined into the function several times, is refused. Nothing
else changes, so the function's variables keep their values. The stop is
shown again at its new place, and a breakpoint there stops the thread as it
resumes, before it runs anything, as gdb's does. Assigning `$pc`, as `set var $pc =
0x401136`, moves the thread anywhere, which is rarely safe. A thread
stopped in a system call that the kernel would restart, as one paused in
`read` is, no longer restarts it once moved.

A forked child is not followed: it runs on its own, without the breakpoints
it inherited. A program that calls `exec` is followed, with its breakpoints.

### Breakpoints

| Command | |
| --- | --- |
| `break`, `b` [*location*] [*options*] | Break at a `function`, `file:line`, `file:function`, `0xaddress`, or a line of the selected frame's file. |
| `tbreak` [*location*] [*options*] | Break once: the stop the breakpoint causes deletes it. |
| `rbreak` *regex* | Break at every function of the loaded modules whose name matches, at most 200. |
| `breakpoints`, `info breakpoints` | List breakpoints and their hit counts. |
| `delete`, `d` *ids...* | Delete breakpoints, and watchpoints written `w2`; `all` deletes every breakpoint. |
| `condition` *id* [*expression*] | Stop only where the [expression](expressions.md) is true; with none, always. |
| `hits` *id* *hit-condition*\|`always` | Replace the hit condition, keeping the count. |
| `ignore` *id* *count* | Skip the next *count* hits. |
| `disable` *ids...* / `enable` *ids...* | Stop using breakpoints and watchpoints, keeping them, and use them again. |
| `save breakpoints` *file* | Write the commands that recreate the breakpoints, for `-c`. |

`condition`, `hits`, and `ignore` change a watchpoint too, named by `w` and
its id: `condition w2 counter > 10`, `hits w2 %100`.

`enable` and `disable` take ids, ranges such as `3-5`, watchpoints such as
`w2` or `w1-3`, or `all` for every breakpoint and watchpoint; a list naming
one that does not exist changes nothing. A disabled breakpoint keeps its
conditions and hit count, counts no hits, and lists the locations it last
had; enabling it finds its locations again, as for a new breakpoint, so it
follows code that moved meanwhile. A disabled watchpoint frees its debug
registers for others, and enabling it may fail when none are left; it reads
the value again, so a change made while disabled is not reported. A
watchpoint keeps its storage while disabled and still ends with it.

A temporary breakpoint counts hits like any other, and the first stop it
causes deletes it, including a stop that several threads reach together.

A breakpoint's options follow its location, in any order, so one command
makes the whole breakpoint:

```text
break parse.c:120 if len > 4
break handle_request hits >=3 log "request {id} from {peer}"
break                     # the selected frame's line
break 42                  # line 42 of the selected frame's file
break +3                  # three lines on; -3 three lines back
```

`disabled` sets a breakpoint that starts disabled. `if` and `log` take the
text up to the next option word, `if`, `hits`, `log`, or `disabled`,
outside a string or brackets, so a condition that uses one of those
words as a name writes it in parentheses. A log message may be quoted,
with `\"` for a quote inside it. A hit condition written right after the
location, as in `break counted ==3`, is the same as `hits ==3`. Lists of
ids, as `delete 1 3-5 w2`, name only breakpoints and watchpoints that
exist, or the command changes nothing.

`breakpoints` lists them in a table: whether each is on (`●`, or `+`
without Unicode) or off (`○`, `-`), its hits, where it is, and its
options as `break` takes them. A breakpoint with several locations lists
them beneath it, each with its address; once the program runs, those are
its runtime addresses. `watchpoints` lists watchpoints the same way.

```text
Id  On  Hits  Where                                 Options
1   ●      3  main at src/basic.c:10                if x > 3
2   ○      0  parse_header, 2 locations             hits >=2  log "len={len}"
              ├ 0x401136  parse_header at src/parse.c:41
              └ 0x7ffff7fb9136  parse_header at src/parse.c:41 in libparse.so
3   ●      0  render at src/view.c:88               temporary
```

`rbreak` matches demangled names anywhere in them, as `rbreak ^parse_`
does, and sets one breakpoint per function, as `break` would; a pattern
matching more than 200 is refused with the count. A function or source
file that no loaded module has is answered with the nearest names, as in
`no function named 'proces' was found; did you mean 'process'?`.

Addresses are always `0x`-prefixed, so `break add` names a function. Functions
without debug information, such as libc's, break at their symbol, and
breakpoints in a shared library wait until it loads; one on a function the
program imports waits without being asked, while a name nothing defines or
imports is refused. C++ and Rust symbols are found by their demangled names,
with or without the scopes and parameters that qualify them: `break scale`,
`break shapes::scale`, and `break shapes::scale(double)` all find
`_ZN6shapes5scaleEd`. An indirect function, such as glibc's `strlen`, stops in
the implementation its resolver chose for the machine, as `__strlen_avx2`,
learned from the slot the loader filled with it or by catching the resolver
as it returns; until the resolver runs, its breakpoint waits. A location
without debug information is described by its symbol, and `address` and
`disassemble` look a symbol up in every loaded module.

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

A session at a terminal keeps its breakpoints for the next one in
`.uscope/state/breakpoints.toml` at the project root, and restores them
when it starts, each pending until code for it loads, so that a
breakpoint saved in one program of a project applies to all of them.
The first save creates `.uscope/state/` with a `.gitignore` of `*`, so
saved state is never committed. Breakpoints are saved as their locations
were written, with a path inside the project relative to its root, and
with their conditions, hit conditions, log messages, and whether they are
enabled; hit counts belong to one process and are not saved, nor are
temporary or address breakpoints. A source breakpoint records the text of
its line, and a session that finds it reading differently restores it
where it was and warns that it changed. Displays are kept in the same file,
as `[[display]]` entries. A file that does not parse is
reported and never overwritten; the session saves nothing until it is
fixed or deleted. Two sessions in one project each save their own
breakpoints, and the last to change them wins. `[breakpoints] save =
false` turns this off, and `--batch` sessions, scripts, and the debug
console neither restore nor save.

Breakpoints and watchpoints can be changed while the program runs: every
thread stops briefly for the change and resumes without a reported stop.

### Watchpoints

| Command | |
| --- | --- |
| `watch` *target* [`if` *condition*] | Stop when a store changes the value. |
| `watch -w` *target* [`if` *condition*] | Stop at every store, even of the same value. |
| `awatch` *target* [`if` *condition*] | Stop at every load or store. |
| `watchpoints`, `info watchpoints` | List watchpoints and their hit counts. |
| `unwatch` *ids...* | Delete watchpoints. |

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
| `backtrace`, `bt` [`-r`] | Show the selected thread's or task's stack; `-r` shows the runtime frames it folds, with their whole names. |
| `frame`, `fr` [*level*] | Show the selected frame, or select one by level. |
| `up` / `down` [*count*] | Select a caller / callee frame. |
| `where` | Show the selected frame's location and module. |
| `list`, `l` | Show source around the selected frame's line, with breakpoint lines marked in the margin. |
| `registers`, `regs` | Show the selected frame's general registers. |
| `context`, `ctx` | Print the sections a stop prints again, in the selected frame. |
| `display` [*expression*] | Print an expression at every stop, with `print`'s formats, as in `display/x n`; alone, list the displays. |
| `undisplay` *ids* | Remove displays: numbers, ranges such as `1-3`, or `all`. |

A stop's first line says why and where, in words, and names its thread when
the process has more than one, and the task the thread runs, if any; a
resume that ran over a second says how long:

```text
stopped at breakpoint 1 (hit 3) in parse_header at src/parse.c:41 [thread 41672 of 4] (ran 1.42s)
```

A runtime's exception says the same, then its message:

```text
stopped as an exception was raised in formatted at src/main.rs:34 [thread 41690 of 3] [3]:
panicked: formatted 7 times
```

Every other thread the stop found at a breakpoint or watchpoint gets a line
of its own after it, with the task it runs:

```text
thread 41673 [7] also stopped at breakpoint 1 (hit 4) in parse_header at src/parse.c:41
```

Breakpoint, step, watchpoint, signal, exception, and pause stops then print
the sections `[stop] show` lists, in order: `source`, the lines around the
stop as `list` shows them; `locals`, as `print` alone; `displays`;
`registers`; `disassembly`, `[stop] disassembly-instructions` around the
instruction; `backtrace`, its first `[stop] backtrace-frames` frames; and
`threads`. The default is `["source"]`. Displays print after the sections when
the list leaves them out. A section that fails prints its error and the others
still print; a stop where no source line is known prints no source, since the
header has given its address.

A display prints as `print` would, after its number, in the selected frame. One
that cannot be evaluated there prints its error, dimmed, rather than
vanishing. Displays are kept with breakpoints, above.

With `[stop] highlight-changes`, a local, display, or register whose value
differs from what the last stop showed of the same activation, the same
function at the same frame address in the same thread, is drawn in the
`changed` role, or followed by `*` without colour. A value is compared only
with itself, so after `up` the caller's values are not compared with its
callee's, and one that was unavailable is never marked.

Backtraces unwind through every loaded module using its own call-frame
information. Frames without debug information are named `symbol+offset` from
the module's ELF symbol tables, including MiniDebugInfo; code no symbol covers
is `<unknown>` rather than borrowing a neighbor's name. A PLT stub is named
after the function it jumps to, as `puts@plt`, and can be broken at by that
name; one whose slot an indirect function's resolver fills is named after
the indirect function. Where a library defines one name several times, as
glibc does an old and a new `memcpy`, versions tell them apart: the old is
`memcpy@GLIBC_2.2.5`, and the default keeps its plain name. A breakpoint on
the plain name takes every version, and one on a versioned name that one. Rust and C++ symbols
are demangled. The vDSO, the code the kernel maps into every process for
calls such as `clock_gettime`, is the module `[vdso]`; no file backs it, so
it is read from the process's memory.

A function that left by a tail call, jumping to another instead of calling
it, has no frame of its own, but a backtrace shows it between the function it
jumped to and the caller, marked `[tail call]`, where the debug information
allows only one chain of tail calls from the call the caller made: the same
chains that recover entry values. The frame is at the jump. The jump
discarded its registers and its place on the stack, so they are
unavailable, as `print $pc` there says; what was passed to it is recovered
as an entry value where the call site that passed it says how. Where more
than one chain is possible, no such frame is shown rather than one guessed.

A Go thread runs the runtime's code on a stack of its own, and signal
handlers on another, and a backtrace follows the runtime from them onto the
goroutine's stack. When a backtrace crosses stacks, each run of frames is
headed by whose stack it is on: the task's (the goroutine's), the runtime's,
the signal stack, or the thread's. The runtime's own functions are dimmed.
Go's calls into C run the C on the runtime's stack, and C's calls back into
Go run the Go on the goroutine's, so a backtrace from either shows the
frames of both languages between them.

A runtime's machinery runs tens of frames deep, so a backtrace folds each
run of two or more of its frames, with the wrappers between them, into one
line, as does everything past the dispatch where the runtime hands the
thread to a task, which is the runtime's code for the thread:

```text
#3  0x00005555555cb472 in top at src/main.rs:94
    … #4–#65: 62 frames of the runtime; `bt -r` shows them
```

Frame numbers count every frame, so `frame 20` selects the same frame
either way, and the frame a stop is in and the selected frame are never
folded. A Rust function's generic arguments, when they run past a few
words, are written `<…>`; `bt -r` shows every frame with its whole name.
All of tokio is its runtime's machinery, the libraries a program awaits
too, as Go's runtime is; so is std's `catch_unwind`, under which a runtime
polls its tasks.

A tokio task that no thread runs keeps its async functions in its future,
and its backtrace is the chain of awaits that future holds, innermost
first: the future it waits on, named by its type and with what it waits
for as its view says, then each async function at the await it is
suspended at, out to the one the task began in:

```text
#0                     awaiting tokio::sync::oneshot::Receiver<u32> — empty
#1  0x00005555555cb77d in async leaf at src/main.rs:66
#2  0x00005555555cca74 in async middle at src/main.rs:84
#3  0x00005555555cb248 in async top at src/main.rs:94
```

An async frame's address is where its function resumes, where the
debugger could find it; it is blank where optimization left no function of
its own to resume in. `print` alone lists what the function keeps across
that await, and an expression reads those variables by name; `$future` is
the frame's future. A suspended frame has no registers and runs no code, so
`registers` and `disassemble` there say so. A task spawned but never polled
is its one async function, at its header. A chain the debugger cannot
follow, as through memory it cannot read, ends where it can, saying why.

A future that `block_on` drives belongs to no task: the thread that blocks
on it holds it between polls. That thread's backtrace shows the future's
chain of awaits just before tokio's frame that drives it, headed by a line
saying so, with each async function's variables as a task's has them:

```text
    … #5–#13: 9 frames of the runtime; `bt -r` shows them
    in the future the next frame drives:
#14                    awaiting tokio::sync::oneshot::Receiver<u32> — empty
#15 0x00005555555d8135 in async waiting at src/main.rs:28
#16 0x00005555555d7db5 in async driven at src/main.rs:36
    on the thread's stack:
#17 0x00005555555cb85f in {closure#0}<…> at …/current_thread/mod.rs:806
```

The future being polled runs on the thread's stack instead, and shows there
as any code does. An optimized build may keep no trace of where the future
is; the backtrace then says so at the frame that drives it:

```text
    the future #6 drives is not shown in full: `f`, which holds the future, is unavailable: the value is optimized out
```
The body of a Go `range` over a function is a function of its own, named
like `main.counted-range1`, which the iterator calls; the iterator's frames
between the body and its loop's function say so, as `(the iterator of #2's
loop)`. The body's frame shows the variables of the loop's function it
uses, and the function's frame shows the rest.

The selected frame applies to `print`, `watch`, `where`, `list`,
`disassemble`, `registers`, and `finish`. Each stop selects the innermost
frame, except that a language runtime's exception selects the frame that
raised it; each thread keeps its own selection until the next stop. An outer
frame's variables are shown as they were at its call, read from where its
callees saved them. A value in a register that callees may overwrite without
saving is reported as not saved rather than shown with the callee's value.
Past Go code, only the stack pointer is recovered.

### Values

| Command | |
| --- | --- |
| `print`, `p` [*expression*] | Print a value, or every parameter and local of the selected frame. |
| `pp` [*expression*] | Print a value laid out to the width, or every parameter and local of the selected frame, expanded. |
| `set` [`var`] *assignment* | Assign, as in `set var x = y + 1`, or a register of the innermost frame, as in `set var $rax = 0`. |
| `whatis` *expression* | Show an expression's type. |
| `ptype` *expression or type* | Show a type's definition. |
| `globals` [*filter*] | List globals and their types without reading them. |
| `info view` *expression* | Explain which view presents a value. |
| `set views on`\|`off` | Present values through views, or as stored. |
| `views` [`load` *file*\|`clear`\|`check`\|`explain` *type*\|`record` *file* *expression*] | Manage view files; see [views.md](views.md). |

Types are named as C declares them, so a pointer to a function shows its
signature, as `int (*)(const char *, ...)`, and its value names the function
it enters, in whichever module holds it: `0x401136 <parse_header>`.

`pp` lays a value out for reading: a group of members or elements that fits
in the rest of the line stays on it, and one that does not puts each member on
a line of its own, indented and ended by a comma. A sequence of plain values
fills its lines instead. The width is the terminal's, measured as each command
runs, or 80 when output is not a terminal, so piped and batch output is the
same everywhere; `[print] width` fixes it.

```text
(uscope) pp *records
(outer_record[2]) *records = [
  {inner = {signed_value = 1, unsigned_value = 2}, values = [3, 4]},
  {inner = {signed_value = 5, unsigned_value = 6}, values = [43, 44]},
]
```

`print` prints on one line unless `[print] style = "pretty"` makes it print as
`pp` does. Both take formats, which combine, as in `p/xr` or `pp/x`: `/x`
prints integers in hexadecimal, members and elements included, and numbers
a view presents, such as an atomic's, `/d` in
decimal, overriding `[print] radix`, `/r` values as stored, without views,
`/p` laid out as `pp` does, and `/l` on one line.

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
| `info modules`, `info sharedlibrary` | List the loaded modules, where each is, what describes its code, and the separate file its debug information came from. |

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
| `tasks` [`-a`] [`-g`] [`-t`] | List a runtime's tasks by number, such as Go's goroutines or tokio's tasks: `-a` with the runtime's own, `-g` grouped by place, `-t` each with its stack. |
| `task` [*id* [*command*]] | Show the selected task, select one, or run an inspecting command in one. |
| `handle` *signal* [`stop`\|`nostop`] [`print`\|`noprint`] [`pass`\|`nopass`] | Change how a signal is handled. `stop` implies `print`, and `noprint` implies `nostop`. |
| `info signals` | List every signal's policy. |
| `catch` [*exception*] [`on`\|`off`] | Show or choose which exceptions language runtimes report stop: Go's `unhandled`, `runtime-fatal`, and `raised`, and `rust-panic`. |

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
then selects again what was selected. `goroutine` alone shows the selected
goroutine and the call that created it, as `created by main.main at
main.go:40`. A tokio task says where it was spawned in a build with
`tokio_unstable`, which records it, as `created at src/main.rs:12`; other
builds record nothing of it. A suspended tokio task is listed with what it
waits for, as the view of the future it awaits says: `sleeping until
+59m59.9s`, `task 3 pending`, `waiting for 1 of 1 permits`, `receiving;
senders: 1`, or `waiting for a notification`.

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

- a panic nothing recovered, as it ends the program (`unhandled`);
- a fatal error, such as `all goroutines are asleep - deadlock!`, or a
  fault the runtime cannot turn into a panic, as one in C is (`runtime-fatal`).

`catch raised on` stops at every panic as it begins too, whether or not
the program recovers from it.

Every Rust panic stops as it begins, before anything catches it, whether a
task's runtime, the program's own `catch_unwind`, or nothing: `rust-panic`,
which `catch rust-panic off` turns off. The stop prints the panic's message,
as the program's panic hook was given it, and selects the program's frame
that panicked, below the standard library's code that raised the panic for
it, as `unwrap` does. A panic whose value is no text, as `panic_any(42)`
raises, is named by its value's type, and `resume_unwind` is reported as
the panic it resumes. With `panic = "abort"`, the panic stops before the
abort it ends in.

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

## Separate debug information

A module stripped of its debug information, as distributions ship their
programs and libraries, takes it from a separate debug file, found as gdb
finds them: by the module's build-id under a debug directory's `.build-id`,
then by the file name and checksum its `.gnu_debuglink` records, beside the
module, in its `.debug` directory, or under a debug directory at the
module's own path. The debug directories are those `--debug-directory` and
`[debug-info] directories` name, then those `NIX_DEBUG_INFO_DIRS` lists,
then `/usr/lib/debug`. With `--debuginfod`, a file no directory holds is
downloaded from the debuginfod servers `DEBUGINFOD_URLS` lists and kept in
debuginfod's cache, which other debuggers share; no server is asked
otherwise. Every candidate must prove it describes the module, by build-id
or checksum. A debug file that uses a dwz supplementary file
(`.gnu_debugaltlink`), as most distributions' do, cannot be read yet: the
module is described by its own file, and `info modules` says why.

## Sources

Source files are read from the paths in the debug information, and read
lazily. For a program built elsewhere, `--source-map FROM TO` reads files
recorded under `FROM` from `TO`, matching whole path components. A missing
source names every path tried. A path recorded without the directory the
program was built in, as a Go `-trimpath` build records
`github.com/you/app/main.go` and `net/http/server.go`, is looked for in the
current directory, and a missing one says so; a rule from
`github.com/you/app` maps it, with or without the leading `./` it is
shown with. uscope does not guess where Go's own sources or the module
cache are. Breakpoints still use recorded paths, or their
trailing components, as in `break main.c:10`.

## Output

Interactive output is colored when stdout is a terminal. `[source] highlight`
colours the keywords, strings, comments, and numbers of C, C++, Rust, Go, and
Zig sources, chosen by the file's extension; a file is read from its start, so
a comment that opened above the lines shown is still coloured. Tabs in source
lines expand to `[source] tab-width` columns. `[ui] hyperlinks` makes each
`file:line` a link to the file in terminals that show OSC 8 links. History is kept in `$XDG_STATE_HOME/uscope/history`, up to `[history]
size` lines. Development builds
record a flight recording of every request and ptrace call under
`target/flight-recorder`; `USCOPE_FLIGHT_RECORDING=PATH` chooses the file,
and an empty value turns it off.
