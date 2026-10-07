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
| `[breakpoints]` | `save` |
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
`tbreak`, `advance`, or `disassemble` (a function, or a file followed by its
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
| `stepi`, `si` / `nexti`, `ni` | Step one instruction, into / over calls. |
| `finish`, `fin` | Run until the selected frame returns. |
| `advance`, `adv` *location* | Run until the selected thread reaches a location, or the selected frame returns. |
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
activation. Stepping always starts from the innermost frame.

`advance` runs until the selected thread reaches any location the *location*
names, as a breakpoint there would stop it, or until the selected frame
returns, whichever comes first, so `advance 42` leaves a loop without
leaving the function. Other threads run meanwhile and pass the location
without stopping. A breakpoint reached on the way stops it as usual.

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
breakpoints in a shared library wait until it loads.

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
frame returns or its block is left. Register values, bit-fields, constants,
and Go stack objects cannot be watched. Watchpoints are discarded when the
process exits or execs, and cleared before detaching.

### Stack and frames

| Command | |
| --- | --- |
| `backtrace`, `bt` | Show the selected thread's stack. |
| `frame`, `fr` [*level*] | Show the selected frame, or select one by level. |
| `up` / `down` [*count*] | Select a caller / callee frame. |
| `where` | Show the selected frame's location and module. |
| `list`, `l` | Show source around the selected frame's line, with breakpoint lines marked in the margin. |
| `registers`, `regs` | Show the selected frame's general registers. |
| `context`, `ctx` | Print the sections a stop prints again, in the selected frame. |
| `display` [*expression*] | Print an expression at every stop, with `print`'s formats, as in `display/x n`; alone, list the displays. |
| `undisplay` *ids* | Remove displays: numbers, ranges such as `1-3`, or `all`. |

A stop's first line says why and where, in words, and names its thread when
the process has more than one; a resume that ran over a second says how long:

```text
stopped at breakpoint 1 (hit 3) in parse_header at src/parse.c:41 [thread 41672 of 4] (ran 1.42s)
```

Breakpoint, step, watchpoint, signal, and pause stops then print the sections
`[stop] show` lists, in order: `source`, the lines around the stop as `list`
shows them; `locals`, as `print` alone; `displays`; `registers`;
`disassembly`, `[stop] disassembly-instructions` around the instruction;
`backtrace`, its first `[stop] backtrace-frames` frames; and `threads`. The
default is `["source"]`. Displays print after the sections when the list
leaves them out. A section that fails prints its error and the others still
print; a stop where no source line is known prints no source, since the
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
is `<unknown>` rather than borrowing a neighbor's name. Rust and C++ symbols
are demangled. The vDSO, the code the kernel maps into every process for
calls such as `clock_gettime`, is the module `[vdso]`; no file backs it, so
it is read from the process's memory.

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
| `print`, `p` [*expression*] | Print a value, or every parameter and local of the selected frame. |
| `pp` [*expression*] | Print a value laid out to the width, or every parameter and local of the selected frame, expanded. |
| `set` [`var`] *assignment* | Assign, as in `set var x = y + 1`. |
| `whatis` *expression* | Show an expression's type. |
| `ptype` *expression or type* | Show a type's definition. |
| `globals` [*filter*] | List globals and their types without reading them. |
| `info view` *expression* | Explain which view presents a value. |
| `set views on`\|`off` | Present values through views, or as stored. |
| `views` [`load` *file*\|`clear`\|`check`\|`explain` *type*\|`record` *file* *expression*] | Manage view files; see [views.md](views.md). |

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
prints integers in hexadecimal, members and elements included, `/d` in
decimal, overriding `[print] radix`, `/r` values as stored, without views,
`/p` laid out as `pp` does, and `/l` on one line.

Expressions are described in [expressions.md](expressions.md). Every value has
an explicit state: available, unavailable for a stated reason (optimized out,
unreadable memory), or invalid for its type. Reads across unreadable memory
report the address that failed.

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
| `threads` | List threads. |
| `thread` *id* | Select a thread. |
| `handle` *signal* [`stop`\|`nostop`] [`print`\|`noprint`] [`pass`\|`nopass`] | Change how a signal is handled. `stop` implies `print`, and `noprint` implies `nostop`. |
| `info signals` | List every signal's policy. |

Signals follow gdb's defaults. `SIGALRM`, `SIGURG`, `SIGCHLD`, `SIGWINCH`,
`SIGPROF`, `SIGVTALRM`, `SIGIO`, and `SIGPWR` are delivered without stopping;
`SIGINT` stops and is discarded; every other signal stops and is delivered on
resume.

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
