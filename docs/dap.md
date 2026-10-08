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

- **Starting.** Launch a program with its arguments, environment, and working directory, optionally stopping at its first instruction. As under gdb, it runs without address randomization, so a rerun shows the same addresses. Attach to a running process, or open a core dump with its module search paths. Loading debug information reports progress.
- **Program I/O.** Output appears in the debug console and input is empty, or the program runs in the client's integrated or external terminal (`"console"`), which owns its input and output.
- **Breakpoints.**
  - Source, function, and instruction breakpoints. A source breakpoint is placed at its line's first statement; a column within the line is not used.
  - Breakpoints in shared libraries, kept pending until the library loads and following it through `dlopen` and `dlclose`. `breakpointLocations` lists the lines of loaded libraries' sources as well as the program's.
  - Functions without debug information, such as libc's, break at their symbol, read demangled for C++ and Rust. An indirect function, such as glibc's `strlen`, breaks in the implementation its resolver chose for the machine.
  - Conditions and logpoints in the [expression language](expressions.md): a logpoint's `{expression}` parts are evaluated at each hit. A condition that does not parse, or that assigns, leaves its breakpoint unverified with the reason; one that fails when evaluated stops the program and says why.
  - Hit counts are an operator and a count: `==5`, `>=5`, `%3`. A bare `5` is refused, since clients disagree about whether it means the fifth hit only or every hit from the fifth.
  - Data breakpoints (hardware watchpoints) on variables, expressions, and addresses. A `write` data breakpoint stops when a store changes the value, as clients present it ("Break on Value Change"); its *On Every Store* mode (`breakpointModes`) stops at every store, even of the value already held. `readWrite` stops at every load and store. A watched local ends with its frame, and the client is told. A goroutine's watched local follows its stack when the runtime moves it. Data breakpoints set before a program is loaded are refused.
  - Conditions and hit counts on data breakpoints, as on breakpoints. A condition is evaluated in the accessing thread's frame after the access, and every reported access is a hit. A data breakpoint re-sent with new conditions keeps its id and its count; one whose conditions do not parse is unverified and no longer watches.
  - Breakpoints can be edited while the program runs. Breakpoints the debug console makes, deletes, disables, or enables are reported to the client, as are the client's data breakpoints it deletes, disables, or enables. A disabled breakpoint stays listed as unverified, with a message naming the console command that enables it.
- **Execution.** Continue, pause, step over, into, and out, by line or by instruction. The debugger is all-stop: every thread stops and resumes together, unless a request names a single thread (`singleThread`). A program that executes itself again is followed with its breakpoints.
- **Step Into Target.** `stepInTargets` lists the calls of the innermost frame's line, from the stopped instruction on, each labeled with the function it calls, or as an indirect call; a caller's frame has none. `stepIn` with one's `targetId` goes into that call, running the line's other calls to their returns, and stops where its function's source begins, or as a plain step in does when the line ends first or the function has no source. A target belongs to the stop that listed it.
- **Jump to Cursor.** `gotoTargets` names the line a thread can be moved to, or the next line with code, as a breakpoint would slide; `goto` moves the thread there without running it, and it stops again with reason `goto`. The line must have code in the function the thread is stopped in, at one place, or `goto` is refused with the reason; the function's variables keep their values.
- **Fork children.** With `followForks`, each process the program forks is debugged in a session of its own, which the adapter asks the client to start with `startDebugging`. See [Following forks](#following-forks).
- **Inspection.**
  - Threads with names. A stopped thread's name says what stopped it, as `worker (4122) — at breakpoint 2` for a thread that hit a breakpoint at the same stop as the one the `stopped` event names. Stack traces go through libraries and inlined calls, with the frames' parameters, lines, and modules when a client asks. A function that left by a tail call is shown, named with `[tail call]`, between the function it jumped to and their caller where the debug information allows only one chain of tail calls; its registers are gone, and its arguments are known where the call sites say what was passed.
  - A Go program's threads are its goroutines, each with its goroutine id as its thread id, named as `[7] main.worker — chan receive {job: resize} (thread 1234)`: the function the program wrote that it is in, past the runtime's machinery, what it waits for or that it stopped at a breakpoint, its profiler labels, and the system thread it is on. A stop names the goroutine that stopped. A parked goroutine's stack, variables, and expressions are its own, and `$task` is its id. The list puts the goroutine that stopped first, then goroutines on threads, then the program's other goroutines, leaving out the runtime's own unless `runtimeTasks` is set; it is cut at `maxTasks`, and a last entry says how many more there are. A system thread that stopped running no goroutine is listed too. `"threads": "system"` lists the system threads instead.
  - A tokio task no thread runs has the chain of awaits its future holds as its stack: a first frame named `awaiting` and the future it waits on, then each async function, named `async leaf`, at the await it is suspended at, out to the one it began in. An async function's locals are what it keeps across that await, and `$future` is its future. These frames have no registers, so they offer none, and an instruction pointer only where the function resumes is known. A thread that blocks on a future, as `block_on` does, has the same frames for it, under a label `in the future the next frame drives`, before tokio's frame that drives it; where optimization lost the future, a label says why instead.
  - A stack that crosses from one stack to another, as Go's runtime does from its own stacks and signal handlers to a goroutine's, has a label heading each run of frames saying whose stack it is on. The runtime's own frames and compiler wrappers are subtle, as are the frames of an iterator that runs the body of a Go `range` over a function, whose names say whose loop they iterate.
  - Arguments, locals, statics, and registers, with the text of strings. Each row's `evaluateName` reaches exactly that variable: a static that a local shadows is named from the outermost scope, such as `::count`, or with its file, such as `` ::`main.c::count` ``, and a variable an inner block hides has none. A register's row is named `$rax`; the innermost frame's can be set, while a caller's, which unwinding recovered, are read-only. A Go function's results, such as `~r0`, are among its arguments. After a step out of a function, the locals of the frame it returned to include what it returned, as `returned count` for a Go result or `returned add` for a C function's value, which no expression names; the CLI's `finish` documents which languages' values are known.
  - Containers presented by [views](views.md): a vector's elements as indexed variables, paged by the client's `filter`, `start`, and `count`, and its fields and `[raw]`, the value as stored, as named ones.
  - Hover, watch, clipboard, and debug console evaluation in the [expression language](expressions.md).
  - Integers in hexadecimal, per request with `format` or for the session with the `uscope/setValueFormat` request (`{"hex": true}`).
  - Each variable's declaration (`declarationLocationReference`), and the function a function pointer points to (`valueLocationReference`), through the `locations` request.
  - Changing values with `setVariable` and `setExpression`, registers among them. Setting `rip` moves the thread, which then stops again with reason `goto`.
  - Memory reads and writes, and disassembly.
  - Modules with their address ranges and symbol files, the vDSO among them as `[vdso]`, and loaded sources. Once a client asks for the loaded sources, `loadedSource` events keep its list current as libraries load and unload.
- **Signals.** Exception filters choose which signals stop the program (`fatal`, `interrupt`, `routine`, `other`), and `exceptionInfo` explains a stop. The `signals` setting overrides the policy of individual signals. A language runtime that handles faults itself, as Go's turns them into panics, gets them silently unless the `signals` setting says otherwise.
- **Exceptions.** Further filters stop where a language runtime reports an exception: `unhandled`, a panic nothing recovered, and `runtime-fatal`, a fatal error such as a deadlock, both on by default; and `raised`, every panic as it begins, off; and Rust's `rust-panic`, every panic as it begins, on. Each runtime declares its own filters, and all are listed whatever the program. The stop selects the frame that panicked: the sources of the runtime's frames above it are deemphasized, so clients focus it and show the exception there. `exceptionInfo` gives the runtime's message, with an expression for the panic's value as `details.evaluateName`. A panic as it begins has no message yet when its value is an error or a stringer of the program's, whose method only the program can run, and is named by the value's type; the runtime's own errors read as their methods put them. A breakpoint instruction of the program's own stops too, and the program goes on past it.
- **Debug console.** A line is an [expression](expressions.md) evaluated in the focused frame, such as `p->items[i]`, `(u8)flags`, `$rip`, or an assignment like `x = 5`, after which the client reads its variables again. A line that starts with a uscope command's name runs the command, such as `info breakpoints`, `print/x mask`, `ptype struct node`, `x 0x7ffd1000 16`, or `handle SIGUSR1 nostop`, unless the frame has a variable of that name: with a local `list`, the lines `list` and `list + 1` read the variable, while `print list` always runs the command. Commands that run the program are refused; use the client's controls. So are `edit`, `display`, and `undisplay`, since the client shows files and watches values itself. A mistake in an expression points at the text it is about. Completion offers commands and their arguments, the frame's variables, globals, members after `.` and `->`, and registers after `$`.
- **Session control.** `restart` relaunches a launched program and keeps its breakpoints. An attached process or a core dump cannot be restarted, which the adapter tells the client with a `capabilities` event; the client restarts the session itself, and the disconnect that begins that restart detaches from an attached process even when it asks to terminate it. `terminate` asks the program to exit, and `cancel` cancels slow requests.
- **Numbering.** Lines and columns follow the client's `linesStartAt1` and `columnsStartAt1`.

### What it does not support

The adapter does not advertise these:

- Restarting a frame (`restartFrame`) and stepping backwards.
- Following children made by `vfork` or `posix_spawn`, which share their parent's memory until they execute another program: they run on their own.
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
  "viewFiles": ["${workspaceFolder}/app.views"],  // ahead of .uscope/views, the user's, the program's, the built-in
  "debugDirectories": ["/srv/debug"],  // separate debug files, ahead of NIX_DEBUG_INFO_DIRS and /usr/lib/debug
  "debuginfod": false,           // download missing debug files from DEBUGINFOD_URLS
  "disassemblySyntax": "intel",  // or "att"
  "signals": { "SIGUSR1": "nostop", "SIGPIPE": ["stop", "print"] },
  "followForks": false,          // debug forked processes in sessions of their own
  "threads": "tasks",            // a runtime's tasks, such as Go's goroutines, as threads; or "system"
  "runtimeTasks": false,         // list the tasks a runtime runs for its own work too
  "maxTasks": 1000               // the most tasks listed
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

- A module stripped of its debug information takes it from a separate debug file: by build-id under a debug directory's `.build-id`, by `.gnu_debuglink` beside it, in its `.debug` directory, or under a debug directory, and, with `debuginfod`, from a debuginfod server. The `modules` request names the file as a module's `symbolFilePath`.
- `pid` may be a number or a numeric string, as VS Code's `${command:pickProcess}` produces.
- Attaching continues the process unless `stopOnEntry` is set.
- Disconnecting detaches from an attached process and kills a launched one, unless the client asks otherwise with `terminateDebuggee`.
- Signal actions are `stop`, `nostop`, `print`, `noprint`, `pass`, and `nopass`.
- Invalid configurations are refused with the path of the offending key, such as ``invalid launch configuration at env.PATH: invalid type: integer `1`, expected a string``.
- Keys clients add, such as `name` or `__sessionId`, are ignored.

### Following forks

`"followForks": true`, in a launch or attach configuration, debugs every process the program forks in a session of its own. The child runs no instruction before its session has attached to it and set its breakpoints, so a breakpoint on the line after `fork()` stops in the child too.

- **How.** The debugger removes the parent's breakpoints from the child, stops it, and releases it untraced. The adapter then asks the client to start a child session with `startDebugging`, as an attach configuration naming the child (`"name": "app (fork 1234)"`, `"pid"`), the parent's `type`, `followForks`, `sourceMap`, `viewFiles`, `debugDirectories`, `debuginfod`, `disassemblySyntax`, `signals`, and `cwd`, an attach's `program`, and `"held": {"startTime": …}`, which tells the child's session to end the stop the child waits in. The child session continues the child once it is configured, unless `stopOnEntry` is set.
- **Clients.** It needs a client that starts child sessions (`supportsStartDebuggingRequest`), such as VS Code and nvim-dap. With any other, the adapter says so once and the children run on their own. Each child session runs its own adapter, and ends independently of the parent's.
- **Children no session takes run on their own.** The adapter releases a child the client refuses to debug or does not answer for within 60 seconds, and one no session has attached to 60 seconds after the client answered, and says so. A child session that fails to attach releases its child at once. A child forked while the parent's session ends is released too. A released child receives the SIGCONT that ends its stop, as after a shell's `fg`.
- **Yama.** A child session attaches to a process that is not its adapter's descendant, which Yama refuses while `kernel.yama.ptrace_scope` is 1, Ubuntu's default, or more. Set it to 0 (`sudo sysctl kernel.yama.ptrace_scope=0`), or, at 1 or 2, give `uscope` the `cap_sys_ptrace` capability. A child session that cannot attach releases the child, which runs on its own.
- **Output.** A launched program's children share its output pipes, which the parent's adapter reads into its debug console until the parent's session ends. A child still writing after that writes to a closed pipe, which raises SIGPIPE; programs whose children outlive them should run in a terminal (`"console": "integratedTerminal"`).
- **Not followed.** Children made by `vfork` or `posix_spawn` share their parent's memory until they execute another program, and run on their own.

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

The test suite drives the real adapter as VS Code, nvim-dap, Helix, and dape do, checks every message against the protocol's schema and the ordering rules strict clients rely on, replays traffic recorded from VS Code and nvim-dap (`tests/dap/traffic`), and compares stepping with gdb's adapter. Two recipes drive the real clients and record their traffic:

```sh
just uat-vscode                  # a VS Code window drives the adapter (needs a display)
just uat-nvim ~/src/nvim-dap     # a headless Neovim drives it with nvim-dap
just uat-vscode tests/dap/traffic   # refresh the recorded VS Code traffic
just uat-nvim ~/src/nvim-dap tests/dap/traffic
```

The VS Code run takes about ten seconds and uses VS Code's own commands wherever a user has one; `editors/vscode/test/uat.js` says what it covers. Before a release, check by hand in VS Code what those runs cannot see:

- Breakpoints in the gutter turn solid once their library loads, and hollow with a reason when they cannot resolve.
- The Variables view pages through a large array, and expands pointers, records, and registers.
- *Open Disassembly View* scrolls both ways and shows padding rows beyond readable memory.
- *View Binary Data* on a variable opens the hex editor at its address, and edits write memory.
- *Break on Value Change* in the Variables view stops when the value changes, and *Edit Mode* on it offers *On Every Store*.
- Inline values appear at the ends of lines, and a function pointer's value links to its function.
- *Copy Value* copies what the Variables view shows.
- A core dump opens with its module warnings in the debug console.
- With `followForks`, a forked child's session appears under its parent's in the Call Stack view, and stopping either leaves the other as it was.
- Console commands complete with Tab and print the same output as the CLI.
