# Command line experience: design

The CLI works but makes the user do the reading: stops print absolute paths
and an address without the function, `breakpoints` lists image addresses
rather than source lines, a mistyped name gets no suggestion, nothing
completes, and every user re-teaches it their preferences on each launch.
This plan makes the CLI configurable and pleasant without moving
presentation into the debugger: everything here is the CLI's, except
enabling, disabling, temporary breakpoints, and `advance`, which change what
the debugger does and land in the core, the simulator, and DAP.

## Goals

- A user states their preferences once, in a standard place, and a project
  can state its own, which win, including how its programs are launched,
  as VS Code's `launch.json` does.
- Breakpoints are managed as a set: listed as a table of source locations,
  enabled and disabled without losing their hit counts, made temporary,
  saved with the project, and restored the next time.
- A stop shows what the user chose to see, says where it is in words, and
  marks what changed.
- `pp` prints a value laid out for reading; `p` can be made to do the same.
- Typing is helped: completion, suggestions for mistakes, aliases.

## Non-goals

- A TUI. Everything here prints lines and stays usable in batch mode,
  pipes, and the DAP console. The TODO's "richer interactive or TUI client"
  is separate.
- Command lists attached to breakpoints (gdb's `commands`). Log messages
  cover printing at a hit, and a breakpoint that runs commands which resume
  is a run-control feature of its own.
- Scripting the CLI beyond its command language.

## Configuration

### Files and precedence

Three files, all optional, in TOML:

1. The user's, `$XDG_CONFIG_HOME/uscope/config.toml`, or
   `~/.config/uscope/config.toml` when `XDG_CONFIG_HOME` is unset or empty,
   as the user's view files are found today. It is the user's alone and
   applies to every project.
2. The project's, `.uscope/config.toml` at the project root, usually
   committed and shared by everyone who works on the project.
3. The project's local file, `.uscope/config.local.toml`, for one person's
   settings in one project. It is meant to be listed in the project's
   `.gitignore`, which uscope never edits.

Each setting comes from the first of these that sets it: a command-line
flag, the environment (`NO_COLOR`, `CLICOLOR`, and `CLICOLOR_FORCE`), the
local file, the project's file, the user's file, and the built-in default.
`PAGER`, `VISUAL`, and `EDITOR` name the pager and editor only where no
file names one, since a file's setting is the more specific choice. Tables merge key by key, so a project that sets `[print]
style` keeps the user's `[print] max-depth`. Lists, such as `[stop] show`,
replace rather than append, because a merged list cannot express removing
an entry. The exceptions are startup commands, which run in order from
every file, and launch configurations, which merge by name; see below.

Every file accepts every setting except `[projects] trust`, which only the
user's may set. A project that is not yet trusted gets its presentation
settings at once and its commands only once the user trusts it; see
[Trusting a project](#trusting-a-project).

`--config FILE` reads `FILE` in place of the user's file, and
`USCOPE_CONFIG=FILE` does the same. An empty `USCOPE_CONFIG`, or
`--no-config`, reads none of the files. Tests set it empty, so that a
developer's own configuration can never change a test's outcome.

### The project root

One definition serves configuration, view files, and saved breakpoints:
the nearest directory, from the start directory upwards, that contains
`.uscope/`; failing that, the nearest that contains `.git` (a directory, or
the file a worktree has); failing that, the start directory. The start
directory is the one uscope runs in, and for DAP the launch request's
`cwd`. A program's `--cwd` changes only where the program runs.

This moves view discovery from `<working directory>/.uscope/views` to the
project root's, and from the program's working directory to uscope's own,
which is a deliberate change: running uscope from a subdirectory of a
project should find the project's views, and a program's `--cwd` says
nothing about which project is being debugged. `docs/views.md` changes with
it.

### Launch configurations

A project describes how its programs are debugged, as `launch.json` does:

```toml
[[launch]]
name = "server"
program = "build/server"            # relative to the project root
args = ["--port", "8080"]
env = { RUST_LOG = "debug" }
cwd = "."
startup = ["break handle_request", "run"]

[[launch]]
name = "attach-server"
attach = "server"                   # a process id, or a name matched as pgrep -x does
startup = ["bt"]

[[launch]]
name = "crash"
core = "core.server"
program = "build/server"
```

A configuration's `cwd` defaults to the project root, as its paths are
relative to it. `--attach` takes a name too, matched the same way.

`uscope --launch NAME` (`-l NAME`) starts one. With no executable, no
`--attach`, and no `--core`, a project with exactly one launch
configuration starts it, and one with several lists their names and
exits, since choosing one would be a guess. Flags change the
configuration they start: arguments after `--` replace its `args`, `--env`
adds to and overrides its `env`, and `--cwd` replaces its `cwd`. An
`attach` name matching more than one process lists them and fails. Each
file's configurations merge by name, so a local file can override the
team's `server` without copying the others. The other options a flag
takes, such as `sysroot`, `module-path`, and `views`, are keys of a
launch configuration too.

Startup commands run in order: the user's `[startup]`, the project's, the
local file's, the launch configuration's `startup`, then `-c` files and
`-e` commands. Unlike other lists they accumulate, because a team's
startup commands should not remove the user's own.

DAP has its own launch configurations in the editor, and `uscope dap`
reads none of these.

### Trusting a project

A project's files arrive with a clone, so one file the user did not write
could change a session's behavior just by being opened. That matters most
on attaching: a committed `set var` or `handle SIGTERM nopass` would
quietly change a live service. VS Code meets the same problem in
`launch.json` with Workspace Trust, and direnv and mise with an `allow`
or `trust` step; uscope does as they do rather than refusing commands
from projects, as gdb's `.gdbinit` auto-load safe path does.

Only settings that act need trust: `[startup]`, `[aliases]`, and
`[[launch]]`, which runs a program with an environment that may preload
code, in the project's file or its local file. Presentation settings,
`[signals]`, and `[[source-map]]` apply without it.

The user's `[projects] trust` chooses the policy:

- `ask`, the default. An interactive session shows a project's acting
  settings, exactly as written, and asks whether to trust them: `yes`,
  `once` (this session only), or `no`. `yes` records them in
  `$XDG_STATE_HOME/uscope/trust.toml` against the project root, and the
  session never asks again until they change. The record holds the
  settings themselves, not a hash, so it is readable and needs no hashing
  crate. A change to a presentation setting does not ask again.
- `always`, for users who want every project's files to apply, as they
  would run its `Makefile`.
- `never`: acting settings in project files never apply.

Declining is never silent: the session starts without the untrusted
settings and warns once which it left out. A batch session, or one whose
input is not a terminal, cannot ask, so an untrusted project with acting
settings is an error naming `--trust-project`, which trusts the project
for that session. `uscope --launch NAME` from an untrusted project asks
first, since the configuration is what the user asked to run.

### Strictness

An unknown key, a wrong type, or a value out of range is an error that
names the file, line, and column, and suggests the nearest valid key
(`unknown key 'colour' in [ui]; did you mean 'color'?`). An invalid file
stops startup rather than being skipped: a session that silently ignores
half of the user's preferences is the convincing wrong result the project
avoids. `--no-config` starts anyway. Keys are kebab-case, as flags are.

### Settings

```toml
[ui]
color = "auto"            # auto | always | never; --color wins
theme = "default"         # default | light; [theme] overrides single roles
unicode = "auto"          # auto (from the locale) | always | never
hyperlinks = "auto"       # OSC 8 links on file:line; auto = terminals known to support them
paths = "relative"        # relative (to the project root) | absolute | name
pager = "auto"            # auto: $PAGER or `less -FRX` when output outgrows the screen | never
editor = ""               # for `edit`: "code -g {path}:{line}"; empty uses $VISUAL, $EDITOR +{line}
prompt = "(uscope) "
confirm-quit = true       # ask before killing a launched program that is still alive

[theme]                   # roles: prompt, command, name, type, value, changed, keyword, string, ...
value = "bright-cyan"
changed = "bold yellow"   # named, 0-255, or #rrggbb, with bold, dim, italic, underline

[source]
context = [3, 3]          # lines before and after the current line, at stops and in `list`
highlight = true          # syntax-highlight C, C++, Rust, Go, and Zig sources
tab-width = 8

[stop]
show = ["source"]         # in order, after the header: source, locals, displays,
                          # registers, disassembly, backtrace, threads
backtrace-frames = 5
disassembly-instructions = 6
highlight-changes = true
elapsed = true            # "ran 1.42s" after a resume that took over a second

[print]
style = "compact"         # compact | pretty: what `p` does; `pp` is always pretty
radix = "decimal"         # decimal | hexadecimal
width = "terminal"        # or a column count; pretty printing fits values to it
indent = 2
max-depth = 64            # aggregate levels expanded
max-elements = 256        # children shown per aggregate

[disassembly]
syntax = "intel"          # intel | att; --disassembly-syntax wins
show-bytes = true

[breakpoints]
save = true               # keep breakpoints in the project's .uscope/state

[history]
size = 10000              # $XDG_STATE_HOME/uscope/history, as today

[signals]
SIGUSR1 = "nostop noprint pass"

[[source-map]]
from = "/build"
to = "."                  # relative to the project root

[aliases]
bb = "break"

[startup]
commands = ["handle SIGPIPE nostop noprint"]

[projects]                # user file only
trust = "ask"             # ask | always | never

[[launch]]                # see Launch configurations
```

`max-depth` and `max-elements` replace `InspectionLimits::aggregate_depth`
and the CLI's `MAX_EXPANDED_CHILDREN` for the CLI's requests, within the
same ceilings the debugger enforces. Text length stays the debugger's
`TextSummary::MAX_BYTES`; making it configurable is a core change left for
later.

### `uscope config`

- `uscope config path` prints the files that would be read, the project
  root, and which exist.
- `uscope config show` prints every setting in effect and where it came
  from (`default`, `user`, `project`, `flag`, `environment`).
- `uscope config check` validates every file and exits non-zero on an
  error, for a project's CI.
- `uscope config trust` and `uscope config untrust` record and forget the
  current project's acting settings, and `uscope config trusted` lists
  every trusted project.
- `uscope config init` writes a user file listing every setting commented
  out at its default, and refuses to overwrite one.

### Code

The configuration is parsed at the edge, in `src/cli/config.rs`, with the
`toml` crate (MIT or Apache-2.0) and serde, as `deny_unknown_fields`
structs whose every field is optional, so that layering is a field-by-field
`or`. The result is one immutable `Settings` the CLI is built with, which
replaces `Renderers`, `AssemblySyntax`, and the flag plumbing they have
now. Nothing in the debugger reads it: `[signals]` and `[[source-map]]` are
applied through the requests the flags already use.

DAP reads no configuration in this plan. Its sessions take their settings
from the client's launch arguments, as editors expect; reading
`[signals]` and `[[source-map]]` there too is a follow-up. Its console
shows paths relative to the project root of the launch `cwd`, as the CLI
does, so the two print the same at the same stop.

`USCOPE_CONFIG` is read and removed from the environment before the
runtime starts, as `USCOPE_FLIGHT_RECORDING` is, so a launched program's
environment, and so its stack layout, never depends on it.

## Breakpoints

### Enabling and disabling (core)

`Breakpoint` and `Watchpoint` gain `enabled`, `BreakpointOptions` and
`WatchpointOptions` gain `enabled` (default true, so a saved disabled
breakpoint is restored disabled in one request), and two requests,
`SetBreakpointEnabled { id, enabled }` and `SetWatchpointEnabled { id,
enabled }`, are edits like adding and removing: while the program runs,
every thread stops briefly for them and resumes without a reported stop.

A disabled breakpoint keeps its id, spec, conditions, log message, and hit
count, owns no trap sites, and counts no hits. Disabling it releases its
sites exactly as removing it does, including forgetting the step over a
trap a thread reported there. Enabling resolves its spec again, as adding
does, and installs the result: while disabled it owns no sites, so moved or
unloaded code (`reconcile_sites`) never updates its locations, and
installing the locations it had when disabled could write a trap into
memory that now holds something else. An address breakpoint has nothing to
resolve and is installed at its address, as `break 0x…` is. A disabled
breakpoint still lists the locations it last resolved to, marked as such.

A disabled watchpoint releases its debug registers, which matters with
only four, and keeps watching for the end of its storage, so a local's
watchpoint still ends when its frame returns. Enabling plans its registers
again, which fails with the existing capacity error, leaving it disabled,
when other watchpoints took them; and reads its bytes again as the value
last observed, so stores made while it was disabled are not reported as a
change.

Adding a breakpoint identical to a disabled one makes a new, enabled one:
`identical` compares `enabled` too. Otherwise `break main` after `disable
1` would silently return the disabled breakpoint, and the user would wait
for a stop that never comes.

### Temporary breakpoints (core)

`BreakpointOptions::temporary`: the breakpoint is deleted by the stop it
first causes, in the same state change, so the stop event and the
following `BreakpointsChanged` agree and no client can act on a stop whose
temporary breakpoint still exists. A hit that does not stop, because of a
condition, hit condition, or log message, keeps it. Two threads that hit
it together are both reported in that one stop. It lands in the core
rather than as the CLI deleting after the stop because the DAP console
shares the CLI and the deletion must be one change with the stop.

### `advance` (core)

`advance LOCATION` runs until the selected thread reaches the location or
the selected frame returns, whichever comes first, as gdb's does. It is a
step out of the selected frame whose `StepStart` also carries the
location's addresses as targets: both are plan breakpoints
(`BreakpointOwner::Plan`), so `cleanup_plan_breakpoints` removes them at
whichever stop ends the plan, another breakpoint's included, and nothing
the user can see is created or left behind. The mid-step cleanups that
replace a step out's return trap keep the targets. The stop says which
ended it: `Step { Advance }` at a target, `Step { Out }` at the return.
Like `finish`, it supports the frames `finish` supports.

As built, only the stepping thread ends it at a target, as only it ends a
`finish` at the return address; another thread passes the location as it
passes any plan trap. A thread already standing on a target goes on to
the next arrival, so `advance` to the current line runs round to it. One
that stopped there by a trap steps over it, as any resumed thread does;
one that has yet to execute the trap, as after an attach or an
instruction step, executes it first, an arrival the user's breakpoints
there count and which does not end the advance.

The stop deletes a temporary breakpoint as the last step before it is
published, so a process that ends while the stop forms keeps it.

### DAP

The protocol has no enabled flag, so the client's own enable checkbox keeps
working as it does now: a client omits a disabled breakpoint from
`setBreakpoints`, the adapter removes the debugger's breakpoint, and
enabling it again makes a new one whose count starts at zero, as in every
other adapter. The adapter cannot do better without guessing: an omitted
breakpoint may have been deleted or disabled, and treating every omission
as a disable would leave deleted breakpoints behind.

What reaches DAP is the debugger's state:

- `enable`, `disable`, and `tbreak` work in the debug console, which runs
  the CLI's commands. `advance` runs the program, so the console refuses it
  as it refuses `finish`; the client's controls run the program.
- A disabled breakpoint is a fourth entry state, `State::Disabled`, so the
  existing state comparison in `sync_breakpoints` sends the `breakpoint`
  event with reason `changed` when `enabled` flips; no new kind of change
  is needed. It is reported `verified: false` with the message `disabled;
  enable N in the debug console`. A client breakpoint shares its debugger
  breakpoint with an identical console one, so disabling either is
  reported on both.
- A temporary breakpoint's deletion at its stop reaches the client as a
  `removed` event, as console deletions do now.
- Data breakpoints follow the same rules through `SetWatchpointEnabled`.

### Commands

`break` takes its options inline, in any order after the location, so one
command makes the whole breakpoint:

```text
break parse.c:120 if len > 4
break handle_request hits >=3 log "request {id} from {peer}"
tbreak main
break                     # the selected frame's line
break 42                  # line 42 of the selected frame's file
break +3                  # three lines after the selected frame's line
```

`hits` replaces the bare trailing hit condition, which stays accepted.
Locations already parsed today keep their meaning. `42` and `+3` are
parsed as function names now and fail, since no supported language's
identifiers start with a digit or `+`, so neither form changes a command
that works.
`if` and `log` take the rest of the line up to the next option keyword
outside a string, so an expression containing the word `log` is written in
parentheses.

As built, `-3` breaks three lines back as `+3` does on, and `delete all`
still deletes breakpoints only, as it did, while `unwatch all` deletes
watchpoints: deleting is not undone, so `all` keeps meaning what the
command names, and only `enable all` and `disable all`, which are undone
by each other, cover both. A breakpoint set or listed is described by its
function and line, with its module when that is not the program, as
`dso_apply at library.c:5 in libmodule.so`, and several locations each
show their address too, since inline copies of a line read alike.

- `enable` / `disable` *ids* change breakpoints, and watchpoints named
  `w2`, as `condition` does. *ids* are a list and ranges, `1 3-5 w2`, or
  `all`; `delete` and `unwatch` take the same.
- `tbreak` *location* [*options*] makes a temporary breakpoint.
- `advance` *location*, alias `adv`, runs to a location.
- `rbreak` *regex* breaks at every function of the loaded modules whose
  name matches, one breakpoint each, as gdb's does, and refuses more than
  200 matches with the count, so a pattern like `.` does not install
  thousands of traps. It matches demangled names. It uses the `regex`
  crate (MIT or Apache-2.0).
- `save breakpoints` *file* writes the commands that recreate the
  breakpoints, for `-c`.

`breakpoints` becomes a table, with the source location of each location
from the module that contains it:

```text
Id  On  Hits  Where                              Options
1   ●      3  main at basic.c:10                 if x > 3
2   ○      0  parse_header, 2 locations          hits >=2  log "len={len}"
              ├ parse.c:41 in uscope
              └ parse.c:41 in libparse.so
3   ●      0  init_plugins                       pending
4   ●      0  render at view.c:88                temporary
```

`○` is disabled and `●` enabled, `-` and `+` without Unicode. A breakpoint
with one location shows it on its line; several are listed beneath.
`watchpoints` gets the same layout.

### Suggestions

A name or file that resolves nowhere is answered with the nearest few
names, by edit distance bounded by the name's length, over the loaded
modules' function names and source files: `no function named 'proces';
did you mean 'process' or 'process_all'?`. Unknown commands and settings
are answered the same way. Suggestions are computed only on failure, so
they cost nothing otherwise, and are never acted on.

### Saved breakpoints

With `[breakpoints] save` true, the default, an interactive session keeps
its breakpoints in `.uscope/state/breakpoints.toml` at the project root,
restores them when it starts, and rewrites the file after each change. The
first save creates `.uscope/state/` with a `.gitignore` of `*`, so saved
state is never committed and uscope never edits the project's own
`.gitignore`; `.uscope/config.toml` and `.uscope/views` stay beside it,
meant to be committed.

```toml
version = 1

[[breakpoint]]
location = "parse.c:120"
condition = "len > 4"
hits = ">=3"
log = "len={len}"
enabled = false
line-text = "    if (len > max) {"
```

- A breakpoint is saved as the user wrote its location, so it is restored
  by the same parser, in its project's terms: a path inside the project
  root is saved relative to it.
- Address breakpoints are not saved, because an address does not survive
  a rebuild; nor are temporary breakpoints or watchpoints, which are bound
  to one stop's storage. `break 0x…` says so when saving is on.
- Restoring makes every breakpoint pending, so one naming code another
  program in the project lacks is kept with no locations, not refused; a
  breakpoint saved in one program of a project applies to all of them, as
  an editor's do.
- `line-text` records a source breakpoint's line when it was saved. If the
  line now reads differently, the breakpoint is restored where it was and
  the session warns that `parse.c:120` changed since it was saved. It is
  never moved to where the text went: a guessed location is worse than a
  stale one the user was told about.
- Hit counts are not saved; they belong to a process.
- The file is written to a temporary file and renamed into place. Two
  sessions in one project each write their own set, and the last write
  wins; this is stated in `docs/cli.md` rather than locked against.
- A file that does not parse is reported and left untouched, and the
  session saves nothing until it is fixed or deleted, so a hand edit with
  a typo is never overwritten.

Batch sessions, `--batch`, neither restore nor save, so that a script
behaves the same in every checkout. As built, only a session reading
commands at a terminal keeps breakpoints, since one reading a pipe is a
script too. A location is saved as written, so `break parse.c:120` saves
`parse.c:120` however the debug information records the path, and the
line text is read where the session's source map finds the file. A saved
breakpoint that cannot be restored, such as a hand edit whose condition
no longer parses, is warned about and written back unchanged rather than
dropped. A session that restored any says `restored N breakpoints`.
`break` gains a `disabled` option, so that `save breakpoints` writes one
command per breakpoint, since ids differ between sessions and a later
`disable N` could name the wrong one. DAP sessions do neither: editors keep
their own breakpoints, and two sources of truth for one set would fight.

## Printing

### `pp`

`pp` *expression* prints a value laid out for reading, as Python's pprint
does: a group that fits in the remaining width stays on one line, and one
that does not puts each child on its own line, indented.

```text
(uscope) p config
(Config) config = {name = "uscope", ports = len=3 [80, 443, 8080], limits = {depth = 4, nodes = 512}, tags = len=2 {"a": len=1 [1], "b": len=0 []}}
(uscope) pp config
(Config) config = {
  name = "uscope",
  ports = len=3 [80, 443, 8080],
  limits = {depth = 4, nodes = 512},
  tags = len=2 {"a": len=1 [1], "b": len=0 []},
}
```

`expanded` in `src/cli/value.rs` already walks a value within its limits;
it changes to build a small document tree (text, and groups with an
opening, children, separators, and a closing) instead of a string, and two
printers lay the tree out: compact, which is today's output exactly, and
pretty, a Wadler-style printer that measures each group against the
width. Output stays bounded by `OUTPUT_LIMIT`, and markers such as
`<12 omitted>` and `<truncated: ValueNodes>` are children like any other.

With the walk shared, radix becomes a property of the leaves, so `p/x` and
`pp/x` print the integers of an aggregate in hexadecimal; today `/x`
applies only to a scalar.

- `pp` with no expression prints every parameter and local of the selected
  frame, each laid out as above.
- `p` follows `[print] style`. With `style = "pretty"`, `p` prints as `pp`
  does, and `p/l` prints one line.
- Formats combine: `/x` (hexadecimal), `/r` (as stored, without views),
  `/p` (pretty), `/l` (one line), as in `p/xr` or `pp/x`. `/p` with `/l` is
  refused.
- The width is the terminal's, read when the command runs, or 80 when
  output is not a terminal, so piped and batch output is the same
  everywhere; `[print] width` fixes it.

As built, `/d` joins the formats to override `radix = "hexadecimal"`, and
is refused with `/x`. A sequence whose elements are all leaves fills its
lines rather than taking one per element, as rustfmt lays out short array
elements, so a 300-element vector takes a screen rather than 300 lines;
records and maps keep one member per line. The line editor measures the
terminal after each line it reads and sends the width with it, so a resize
applies to the next command.

## Stops

### The header

A stop's first line says where, in words:

```text
breakpoint 1 (hit 3) in parse_header at parse.c:41 [thread 2 of 4]
```

The function and line come from the innermost frame; the thread is shown
only when the process has more than one; paths follow `[ui] paths`. The
address moves to `where`, `bt`, and the `registers` and `disassembly`
sections, which are where it is read.

### Sections

After the header, a stop prints the sections `[stop] show` lists, in order.
Each is the output of an existing command, made by the same code: `source`
is `list`, `locals` is `p`, `registers` is `registers`, `disassembly` is
`disassemble` around the instruction, `backtrace` is `bt` limited to
`backtrace-frames`, `threads` is `threads`. `context` (alias `ctx`) prints
them again on demand. A section that fails prints its error and the rest
still print. The default, `["source"]`, is what every stop prints today.

### Displays

`display` *expression* adds an expression printed at every stop, in the
selected frame, as `p` would print it, with `/` formats; `display` alone
lists them, `undisplay` *ids*|`all` removes them. One that fails to
evaluate in the frame prints its error, dimmed, rather than vanishing, so
a display that stops working is visible. Displays belong to the CLI and are
saved with breakpoints, as `[[display]]` entries of the same file.

### Changes

With `[stop] highlight-changes`, a local, display, or register whose value
differs from what the last stop showed is drawn in the `changed` role. The
CLI keeps the previous stop's rendered values, keyed by thread, function,
canonical frame address, and name, and compares the text, so a value is
compared only with itself in the same activation, and one that was
unavailable is never marked. Without colour, a changed value is
suffixed with `*` instead.

### Source

- The margin marks breakpoint lines, `●` or `○`, beside `=>`, from the
  locations' source lines, and the current line keeps today's marker.
- `[source] highlight` colours keywords, strings, comments, and numbers
  with a small lexer per language, chosen by the compile unit's language
  and then the extension, rather than syntect's grammars: five languages,
  a few hundred lines, no regex engine at stops. A file is lexed once from
  its start, so a comment opened above the context window is coloured, and
  cached. Highlighting is off whenever colour is.
- `[ui] hyperlinks` makes each `file:line` an OSC 8 link, and `edit` opens
  `[ui] editor` at the selected line.

### Elapsed time

With `[stop] elapsed`, a stop after a resume that ran over a second says
how long: `ran 1.42s`. It is measured in the CLI from the resume's
acknowledgement to the stop's event.

### As built

The header keeps the word `stopped` that every other stop line begins
with, as in `stopped at breakpoint 1 (hit 3) in parse_header at
parse.c:41`, and names the thread by the id `thread` takes, `[thread 41672
of 4]`, rather than its position. Attach and entry stops say where too.
Signal and pause stops print the sections as well as breakpoint, step,
and watchpoint ones, since where a crash happened is what one wants to
see; a stop with no source line prints no source section rather than an
error, as its header gives the address. Displays print after the
sections when `show` leaves them out, so that a display added is seen
without editing the settings. The canonical frame address comes from a
new `VariableSnapshot::frame_address`, read where the frame's variables
are, and a value is compared with what the stop before showed of it, so
a value is unmarked after a stop that did not show it. Elapsed time runs
from the command's request rather than its acknowledgement, which differ
by a message's latency.

## Input

- **Completion** of commands, aliases, settings, function names,
  `file:line` locations, breakpoint ids, and, after `p`, `pp`, `display`,
  and `watch`, the selected frame's names and the fields of a value after
  `.` or `->`. The line editor's thread never calls the debugger: with each
  acknowledgement the REPL sends it a completion context, the stop's names
  and the modules' functions and files built once per module set, so
  completing never waits on, or races with, the controller. Field
  completion asks the debugger for one expression's type when the line
  needs it, through the same acknowledgement channel.
- **Unique prefixes**: a prefix that names one command runs it; a
  configured alias wins over a prefix.
- **A pager** for output taller than the terminal, when interactive.
- **`confirm-quit`**: `quit` while a launched program is alive asks first;
  end-of-input and batch sessions never ask.

### As built

Highlighting chooses the language by the file's extension only, since
the public model names no compile unit's language for a file; `.h` is C.
Tabs expand to `[source] tab-width`, which nothing had applied. Completion
offers command names and the settings' aliases but not the commands' own
short aliases, which abbreviate what they would complete to, and appends
a space to a word completed alone. Expressions are scanned with the debug
console's scanner, `dap::complete`, so both complete alike; a range's
`..` no longer reads as a member access in either. Member completion is
the line editor's one request: it sends the expression to the REPL,
which is waiting on the editor, and waits up to two seconds for the
names. `edit`, `display`, and `undisplay` are refused in the debug
console, whose client shows files and watches values itself.

The pager and the editor are the CLI's only child processes. While a
program is traced the debugger's waiter collects every child's exit, so
the CLI may find its child already collected; it then knows the child
ended but not its status. The controller records such an exit as a
thread that vanished before its clone event, and that record outlives
it: a new thread of the program given the same pid after the pids wrap
would be taken for that vanished thread. Telling the controller which
children are the CLI's would close this, and is left for later.

## Phases

Each phase ends with its tests passing and `just sim 60` where the core
changed; `just all`, `just stress`, and `just sim 600` run once at the end.

1. **Configuration.** Files, the project root, precedence, strict errors,
   `uscope config`, and the existing flags and history moved onto
   `Settings`; themes; view discovery from the project root; launch
   configurations; trust. Tests: one CLI test of precedence across flag,
   local, project, user, and default; one of an error's position and
   suggestion; a launch configuration started by name with its arguments
   and environment, and refused by ambiguity with several; startup
   commands from every file in order; an untrusted project's startup
   commands failing a batch session with a message naming
   `--trust-project`, running with it, and running after `uscope config
   trust` until they change; `never` leaving them out with a warning;
   `USCOPE_CONFIG=""` in every CLI test's environment. The prompt itself is
   tested at the unit level, since a terminal is not driven in tests.
2. **Enable, disable, temporary, advance.** Core requests and options, the
   simulator's client choosing them and its hit oracles knowing a disabled
   breakpoint counts nothing and a temporary one is gone after its stop,
   coverage marks the gate's seeds reach, DAP's disabled state, and the
   commands `enable`, `disable`, `tbreak`, and `advance` in their plain
   forms, which phase 3 extends with inline options and id lists for
   `delete` and `unwatch`. The simulator draws its new choices from a
   stream of their own, `Control`, so the existing seeds keep the runs
   they named and the gate's sabotage tests keep finding their lies.
   Tests, each
   written to fail first: a scenario that disables a breakpoint while the
   program runs, sees it not stop and keep its count, and enables it
   again; one that enables a breakpoint after its library moved and was
   reloaded; a watchpoint re-enabled after the stored value changed
   reports no change; a temporary breakpoint co-hit by two threads; an
   `advance` that ends at another breakpoint leaves no traps (the
   ownership oracle checks this); a DAP scenario of console `disable` and
   the `changed` event.
3. **Breakpoint commands and saving.** Inline options, new location forms,
   id lists for `delete` and `unwatch`, `rbreak`, the tables, suggestions, saved
   breakpoints, `save breakpoints`. Tests: rendered tables; a session that
   saves, ends, and restores a disabled conditional breakpoint; a changed
   line warns; a malformed saved file is not overwritten; batch sessions
   touch nothing.
4. **Printing.** The document tree, both printers, `pp`, combined formats,
   `[print]`. Tests: compact output unchanged byte for byte by the existing
   CLI tests; pretty output of a nested struct, a long vector, and a map at
   two widths; `/x` on an aggregate.
5. **Stops.** The header, sections, `context`, displays, change
   highlighting, margin markers, elapsed time. Tests: a stop with every
   section; a changed local marked after `next` and not after `up`.
6. **Input and polish.** Completion, prefixes, highlighting, hyperlinks,
   `edit`, pager, `confirm-quit`. Tests: completion candidates from a
   context, at the unit level, since a terminal is not driven in tests;
   lexer cases for comments and strings that span lines.

`docs/cli.md` changes with each phase, and `docs/dap.md` with phase 2.

## Open questions

- Whether `[signals]` and `[[source-map]]` should apply to DAP sessions,
  under the launch arguments.
- Whether `rbreak` should follow libraries loaded later, which would need a
  pattern spec in the core rather than expansion in the CLI.
