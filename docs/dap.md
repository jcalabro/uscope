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

- **Starting.** Launch a program with its arguments, environment, and working directory, optionally stopping at its first instruction. As under gdb, it runs without address randomization, so a rerun shows the same addresses. Attach to a running process, or open a core dump with its module search paths.
- **Program I/O.** Output appears in the debug console and input is empty, or the program runs in the client's integrated or external terminal (`"console"`), which owns its input and output.
- **Breakpoints.**
  - Source, function, and instruction breakpoints.
  - Breakpoints in shared libraries, kept pending until the library loads and following it through `dlopen` and `dlclose`.
  - Functions without debug information, such as libc's, break at their symbol.
  - Conditions, hit counts, and logpoints.
  - Data breakpoints (hardware watchpoints) on variables and addresses. A `write` data breakpoint stops when a store changes the value, as clients present it ("Break on Value Change"); `readWrite` stops at every load and store.
  - Breakpoints can be edited while the program runs.
- **Execution.** Continue, pause, step over, into, and out, by line or by instruction. The debugger is all-stop: every thread stops and resumes together. A program that executes itself again is followed with its breakpoints.
- **Inspection.**
  - Threads with names, and stack traces through libraries and inlined calls.
  - Arguments, locals, statics, and registers, with the text of strings.
  - Standard library and user containers presented by views
    (`docs/views.md`): a vector's elements as indexed variables, paged by
    the client's `filter`, `start`, and `count`, and its fields and
    `[raw]`, the value as stored, as named ones.
  - Hover, watch, and clipboard evaluation.
  - Changing values with `setVariable` and `setExpression`.
  - Memory reads and writes, and disassembly.
  - Modules and loaded sources.
- **Signals.** Exception filters choose which signals stop the program (`fatal`, `interrupt`, `routine`, `other`), and `exceptionInfo` explains a stop. The `signals` setting overrides the policy of individual signals.
- **Debug console.** Lines that are not expressions run as uscope commands, such as `info breakpoints`, `disassemble`, `x 0x7ffd1000 16`, or `handle SIGUSR1 nostop`. Commands that run the program are refused; use the client's controls.
- **Session control.** `restart` relaunches the program and keeps breakpoints, `terminate` asks the program to exit, and `cancel` cancels slow requests.

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
- **Configurations.** Pressing F5 in a folder without a `.vscode/launch.json` creates one with a launch configuration to fill in. *Add Configuration…* offers more as *uscope* snippets.
- **Attaching.** `"pid": "${command:pickProcess}"` picks one of your processes when the session starts. With `program` set as well, the picker lists only processes running it.
- **Debugging the adapter.** Run `uscope dap --port 4711` and add `"debugServer": 4711` to the configuration.

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

Before a release, check by hand in VS Code what those runs cannot see:

- Breakpoints in the gutter turn solid once their library loads, and hollow with a reason when they cannot resolve.
- The Variables view pages through a large array, and expands pointers, records, and registers.
- *Open Disassembly View* scrolls both ways, shows padding rows beyond readable memory, and steps by instruction.
- *View Binary Data* on a variable opens the hex editor at its address, and edits write memory.
- *Break on Value Change* in the Variables view stops when the value changes.
- Watch expressions, hover, and *Copy Value* show the same value.
- A core dump opens with its module warnings in the debug console.
- Console commands complete with Tab and print the same output as the CLI.

The same configurations work in nvim-dap, Zed, Helix, and dape as above.
