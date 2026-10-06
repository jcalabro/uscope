# Debugging from an editor

`uscope dap` serves the [Debug Adapter Protocol](https://microsoft.github.io/debug-adapter-protocol/), so editors that speak it can debug native Linux programs with uscope. It speaks the protocol on stdin and stdout by default:

```sh
uscope dap                       # stdio, as editors start adapters
uscope dap --port 4711           # TCP on 127.0.0.1, one client at a time
uscope dap --listen 0.0.0.0:4711 # TCP on another address
uscope dap --log /tmp/dap.log    # append every message to a file
```

Over TCP the adapter refuses a connection that sends an `Origin` header, which only a web page sends.

## What it supports

- **Starting.** Launch a program with its arguments, environment, and working directory, optionally stopping at its first instruction. As under gdb, it runs without address randomization, so a rerun shows the same addresses. Attach to a running process, or open a core dump with its module search paths. Loading debug information reports progress to a client that shows it.
- **Program I/O.** Output appears in the debug console and input is empty, or the program runs in the client's integrated or external terminal (`"console"`), which owns its input and output.
- **Breakpoints.**
  - Source, function, and instruction breakpoints. A source breakpoint is placed at its line's first statement; a column within the line is not used.
  - Breakpoints in shared libraries, kept pending until the library loads and following it through `dlopen` and `dlclose`. `breakpointLocations` lists the lines of loaded libraries' sources as well as the program's.
  - Functions without debug information, such as libc's, break at their symbol.
  - Conditions and logpoints in the [expression language](expressions.md): a logpoint's `{expression}` parts are evaluated at each hit. A condition that does not parse, or that assigns, leaves its breakpoint unverified with the reason; one that fails when evaluated stops the program and says why.
  - Hit counts are an operator and a count: `==5`, `>=5`, `%3`. A bare `5` is refused, since clients disagree about whether it means the fifth hit only or every hit from the fifth.
  - Data breakpoints (hardware watchpoints) on variables, expressions, and addresses. A `write` data breakpoint stops when a store changes the value, as clients present it ("Break on Value Change"); in its *On Every Store* mode (`breakpointModes`), it stops at every store, even of the value already held. `readWrite` stops at every load and store. A watched local ends with its frame, and the client is told. Data breakpoints set before a program is loaded wait unverified.
  - Breakpoints can be edited while the program runs. Breakpoints made or deleted in the debug console are reported to the client, as are the client's data breakpoints the console deletes.
- **Execution.** Continue, pause, step over, into, and out, by line or by instruction. The debugger is all-stop: every thread stops and resumes together, unless a request names a single thread (`singleThread`). A program that executes itself again is followed with its breakpoints.
- **Inspection.**
  - Threads with names, and stack traces through libraries and inlined calls, with the frames' parameters, lines, and modules when a client asks.
  - Arguments, locals, statics, and registers, with the text of strings. Each row's `evaluateName` reaches exactly that variable: a static that a local shadows is named from the outermost scope, such as `::count`, or with its file, such as `` ::`main.c::count` ``, and a variable an inner block hides has no name, since no expression reaches it. A register's row is named `$rax` and is read-only.
  - Hover, watch, clipboard, and debug console evaluation in the [expression language](expressions.md).
  - Integers in hexadecimal, as a request's `format` asks, or for the whole session with the `uscope/setValueFormat` request (`{"hex": true}`), which has the client read its values again.
  - Each variable's declaration (`declarationLocationReference`), and the function a function pointer points to (`valueLocationReference`), through the `locations` request.
  - Changing values with `setVariable` and `setExpression`.
  - Memory reads and writes, and disassembly.
  - Modules with their address ranges and symbol files, and loaded sources. Once a client asks for the loaded sources, `loadedSource` events keep its list current as libraries load and unload.
- **Signals.** Exception filters choose which signals stop the program (`fatal`, `interrupt`, `routine`, `other`), and `exceptionInfo` explains a stop. The `signals` setting overrides the policy of individual signals.
- **Debug console.** A line is an [expression](expressions.md) evaluated in the focused frame: `count * 2`, `p->items[i]`, `(u8)flags`, `$rip`, or an assignment such as `x = 5` or `total += 1`, after which the client reads its variables again. A line that starts with the name of a uscope command runs the command instead, such as `info breakpoints`, `print/x mask`, `whatis p`, `ptype struct node`, `disassemble`, `x 0x7ffd1000 16`, or `handle SIGUSR1 nostop`, unless the frame has a variable of that name: in a frame with a local `list`, the lines `list` and `list + 1` read the variable, while `print list` always evaluates and `list` alone lists source only where no variable is named `list`. A mistake in an expression is shown pointing at the text it is about. Commands that run the program are refused; use the client's controls. Completion offers commands, their arguments, the frame's variables, the program's globals, members after `.` and `->`, and registers after `$`.
- **Session control.** `restart` relaunches a launched program and keeps its breakpoints. An attached process or a core dump has no program to run again, so the adapter tells the client it does not restart them (a `capabilities` event): the client restarts the session itself, and the disconnect that begins a restart detaches from an attached process, even when it asks to terminate it, so that the client can attach to it again. `terminate` asks the program to exit, and `cancel` cancels slow requests.
- **Numbering.** Lines and columns follow the client's `linesStartAt1` and `columnsStartAt1`.

### What it does not support

These need debugger features uscope does not have yet, so the adapter does not advertise them:

- Jumping to a line (`gotoTargets`, VS Code's *Jump to Cursor*), and assigning registers, which both need writable registers.
- Stepping into a chosen call on a line (`stepInTargets`), restarting a frame (`restartFrame`), and stepping backwards.
- Showing the value a function returned after stepping out of it.
- Conditions and hit counts on data breakpoints, which are refused with a reason.
- Following the children of `fork`: they are released and run on their own.
- Terminating single threads, and leaving a process suspended when detaching from it.
- Sending source contents: every source has a path, and the client reads it.

## Configuration

A launch configuration:

```jsonc
{
  "type": "uscope",
  "request": "launch",
  "name": "Launch",
  "program": "${workspaceFolder}/build/app",  // required
  "args": ["--flag"],
  "cwd": "${workspaceFolder}",
  "env": { "RUST_BACKTRACE": "1", "UNWANTED": null },  // over the adapter's environment; null removes
  "stopOnEntry": false,          // stop at the first instruction, in the dynamic loader
  "console": "internalConsole",  // or "integratedTerminal" or "externalTerminal"
  "sourceMap": [["/build/src", "${workspaceFolder}/src"]],  // earlier rules first; {"from": "to"} also works
  "disassemblySyntax": "intel",  // or "att"
  "signals": { "SIGUSR1": "nostop", "SIGPIPE": ["stop", "print"] }
}
```

Attaching to a process, or opening a core dump:

```jsonc
{ "type": "uscope", "request": "attach", "name": "Attach", "pid": 1234,
  "program": "optional/executable", "stopOnEntry": false }

{ "type": "uscope", "request": "attach", "name": "Core dump", "coreFile": "core.1234",
  "program": "optional/executable", "sysroot": "/srv/image", "modulePaths": ["/srv/libs"],
  "allowModuleMismatch": false }
```

- `pid` may be a number or a numeric string, as VS Code's `${command:pickProcess}` produces.
- Attaching continues the process unless `stopOnEntry` is set.
- Disconnecting detaches from an attached process and kills a launched one, unless the client asks otherwise with `terminateDebuggee`.
- Signal actions are `stop`, `nostop`, `print`, `noprint`, `pass`, and `nopass`.
- Invalid configurations are refused with the path of the offending key, such as ``invalid launch configuration at env.PATH: invalid type: integer `1`, expected a string``.
- Keys clients add, such as `name` or `__sessionId`, are ignored.

## Clients

`uscope` must be on `PATH`, or replace `uscope` below with its path.

### VS Code

`editors/vscode` is a small extension, written in plain JavaScript with no build step, that contributes the `uscope` debugger type. It provides configuration completion, snippets, and breakpoints in C, C++, Rust, Go, and Zig. Install it by linking it into the extensions directory, then reload the window:

```sh
ln -s "$PWD/editors/vscode" ~/.vscode/extensions/uscope.uscope-0.1.0
```

- **Finding uscope.** The extension runs `uscope dap` from `PATH`. The `uscope.path` setting names another executable, such as `${workspaceFolder}/target/debug/uscope`; it may start with `${workspaceFolder}` or `${userHome}`.
- **Configurations.** Pressing F5 in a folder without a `.vscode/launch.json` creates one with a launch configuration to fill in. *Add Configuration…* offers more as *uscope* snippets. The Run and Debug view's list of configurations also offers to launch each program built in the folder: the x86-64 ELF executables within four directories of it, outside hidden directories and dependencies' build directories such as `node_modules` and Cargo's `deps`.
- **Attaching.** `"pid": "${command:pickProcess}"` picks one of your processes when the session starts. With `program` set as well, the picker lists only processes running it. *Restart* detaches and attaches to the same process again.
- **Hovers.** Hovering a name evaluates the whole expression it ends, such as `p->items[i].next` or `ns::config.limit`.
- **Inline values.** With VS Code's `debug.inlineValues` setting at its default, each variable of the stopped frame shows its value at the end of the lines that use it, from its declaration to the stop. Lines in blocks that close before the stop are left out, since a name there may be another variable, as are variables an inner block hides.
- **Hexadecimal.** *Toggle Hexadecimal Display*, in the Variables and Watch views' context menus and the command palette, flips the `uscope.hexadecimal` setting, which every session follows.
- **Debugging the adapter.** The `uscope.logFile` setting has the adapter append every message to a file, as `uscope dap --log` does. Or run `uscope dap --port 4711` and add `"debugServer": 4711` to the configuration.

This repository's `.vscode/launch.json` debugs a few test programs: crashes in C, Go, and Rust, a C core dump, Rust variables of many types, and a program that reads from the integrated terminal. Each builds uscope and the programs first, and `.vscode/settings.json` points `uscope.path` at the build.

### Neovim (nvim-dap)

```lua
local dap = require('dap')
dap.adapters.uscope = { type = 'executable', command = 'uscope', args = { 'dap' } }
dap.configurations.c = {
  {
    type = 'uscope',
    request = 'launch',
    name = 'Launch',
    program = function() return vim.fn.input('Program: ', vim.fn.getcwd() .. '/', 'file') end,
    cwd = '${workspaceFolder}',
  },
  {
    type = 'uscope',
    request = 'attach',
    name = 'Attach',
    pid = require('dap.utils').pick_process,
  },
}
dap.configurations.cpp = dap.configurations.c
dap.configurations.rust = dap.configurations.c
dap.configurations.zig = dap.configurations.c
```

`"console": "integratedTerminal"` runs the program in a terminal buffer.

### Helix

In `languages.toml` (untested; based on Helix's configuration format):

```toml
[[language]]
name = "c"

[language.debugger]
name = "uscope"
transport = "stdio"
command = "uscope"
args = ["dap"]

[[language.debugger.templates]]
name = "launch"
request = "launch"
completion = [{ name = "program", completion = "filename" }]
args = { program = "{0}" }

[[language.debugger.templates]]
name = "attach"
request = "attach"
completion = ["pid"]
args = { pid = "{0}" }
```

Helix does not support `runInTerminal`, so `"console"` must stay `"internalConsole"`.

### Zed

Zed runs custom adapters through an extension that declares them (untested). The extension's `extension.toml` declares the adapter:

```toml
[debug_adapters.uscope]
```

Its `get_dap_binary` returns the command `uscope` with the argument `dap`. A `.zed/debug.json` entry then uses it:

```json
[
  { "adapter": "uscope", "label": "Launch", "request": "launch", "program": "$ZED_WORKTREE_ROOT/build/app" }
]
```

### Emacs (dape)

```elisp
(add-to-list 'dape-configs
             `(uscope
               modes (c-mode c-ts-mode c++-mode c++-ts-mode rust-mode rust-ts-mode zig-mode)
               command "uscope"
               command-args ("dap")
               :type "uscope"
               :request "launch"
               :program dape-buffer-default
               :cwd dape-cwd))
```

(Untested.) dape serializes `nil` as `false`; use `:null` for a JSON null, such as to remove an environment variable.

## Testing with real clients

The test suite drives the real adapter as each supported client does. It checks every message against the protocol's schema and the ordering rules strict clients rely on. It also replays traffic recorded from VS Code and nvim-dap (`tests/dap/traffic`), and compares stepping with gdb's adapter. Two recipes drive the real clients and record their traffic:

```sh
just uat-vscode                  # a VS Code window drives the adapter (needs a display)
just uat-nvim ~/src/nvim-dap     # a headless Neovim drives it with nvim-dap
just uat-vscode tests/dap/traffic   # refresh the recorded VS Code traffic
just uat-nvim ~/src/nvim-dap tests/dap/traffic
```

The VS Code run takes about ten seconds. It uses VS Code's own commands wherever a user has one, and checks what VS Code asks the adapter and is answered:

- Starting: creating a launch.json, the `uscope.path` and `uscope.logFile` settings, the programs offered to launch, and configurations refused with their reasons (a missing program, invalid arguments, a process or core dump that does not exist).
- Breakpoints: conditions, hit counts, logpoints, function breakpoints, breakpoints added while running, breakpoints in a library that loads later, and those refused with their reasons.
- Execution: stepping in, over, and out, by instruction from the disassembly view, run to cursor, pause, threads, a crash and its exception, restarting a launched program, an attached process, and a core dump.
- Inspection: the debug console's expressions, assignments, commands, and mistakes, the watch view, hovers, hexadecimal display, inline values, completions, function pointer links, data breakpoints and their modes, and loaded sources.
- I/O: a program in the integrated terminal, attaching through the process picker.

The replay tests use the `vscode-launch`, `vscode-terminal`, `vscode-attach`, and `vscode-core` recordings; the run writes one for every session.

Before a release, check by hand in VS Code what those runs cannot see:

- Breakpoints in the gutter turn solid once their library loads, and hollow with a reason when they cannot resolve.
- The Variables view pages through a large array, and expands pointers, records, and registers.
- *Open Disassembly View* scrolls both ways and shows padding rows beyond readable memory.
- *View Binary Data* on a variable opens the hex editor at its address, and edits write memory.
- *Break on Value Change* in the Variables view stops when the value changes, and *Edit Mode* on it offers *On Every Store*.
- Inline values appear at the ends of lines, and a function pointer's value links to its function.
- *Copy Value* copies what the Variables view shows.
- A core dump opens with its module warnings in the debug console.
- Console commands complete with Tab and print the same output as the CLI.

The same configurations work in nvim-dap, Zed, Helix, and dape as above.
