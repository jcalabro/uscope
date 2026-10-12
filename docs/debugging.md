# Debugging

uscope can launch a program, attach to a running process, or inspect a core
dump. The terminal, browser, and editor interfaces use the same debugger.

## Launch a program

Pass the executable before `--` and its arguments after it:

```sh
uscope ./server -- --port 8080
```

Use `--cwd DIR` to choose the program's working directory and `--env
NAME=VALUE` to add environment variables. A launched program stops before it
begins. Set breakpoints, then run it.

## Attach to a process

Attach by process ID or by a name that matches exactly one process:

```sh
uscope --attach 1842
uscope --attach my-server
```

uscope normally finds the executable through `/proc`. Supply it as the
positional executable when that is not possible.

Quitting an attached session detaches and leaves the process alive. Killing an
attached process requires an explicit command.

## Open a core dump

```sh
uscope --core core.1842
```

uscope uses the executable recorded in the dump when it can find a matching
file. For a dump copied from another machine, `--sysroot` points at a copy of
that machine's file tree. `--module-path` adds directories to search for
missing modules.

A core dump is read-only. You can inspect threads, stacks, values, memory, and
instructions, but cannot resume or modify the program.

## Set breakpoints

Break on a function, source location, or address:

```text
break handle_request
break server.c:84
break *0x401240
```

Add `if EXPRESSION` for a condition:

```text
break handle_request if request.id == 42
```

Use `info breakpoints` to list breakpoints, and `disable`, `enable`, or
`delete` with a breakpoint number to manage one. `tbreak` creates a temporary
breakpoint removed after its first hit.

`advance LOCATION` runs to a temporary source location in the current frame.

## Use watchpoints

A watchpoint stops when the CPU writes to an address:

```text
watch counter
watch buffer[3]
```

Watchpoints use the machine's limited hardware debug registers. The watched
expression must resolve to an address and a supported size. A watchpoint stays
at that address; if the variable later lives elsewhere, create a new one.

## Step through code

```text
step       enter calls on the next source line
next       run the next source line without entering calls
finish     run until the selected frame returns
stepi      run one machine instruction
nexti      run one instruction, stepping over a call
continue   resume until another stop
```

Source stepping follows inline calls and skips compiler-generated code where
the debug information identifies it. In optimized code, one instruction may
belong to several source locations or none.

The debugger is all-stop: when one thread stops, all threads stop before a
state is shown. Resuming runs them together.

## Inspect the stack

`bt` shows the selected thread's frames. `frame N`, `up`, and `down` select a
frame. Expressions and locals use the selected frame's scope.

```text
bt
frame 2
locals
print request
```

Use `threads` to list native threads and `thread ID` to select one. Programs
with a supported runtime can also expose logical tasks such as goroutines and
async Rust tasks. See [Languages](languages.md).

## Inspect values

`print EXPRESSION` evaluates an expression. `pp` expands it in a multiline
form. Format suffixes choose common numeric displays:

```text
print/x flags
print/d address
print/t mask
```

uscope uses one [expression language](expressions.md) for every source
language. [Views](views.md) make containers, strings, maps, and sum types read
as their logical values.

When a variable was optimized out, a memory read failed, or debug information
cannot describe a value, the output states that reason.

## Inspect low-level state

Use `registers` for the selected frame's registers, `disassemble` for nearby
instructions, and `x` to examine memory. The command reference documents their
forms and output controls.

## Handle signals

`signals` lists signal policy. A signal that stops one thread is kept for that
thread while the debugger gathers the all-stop state. Continue can deliver or
suppress it according to the configured policy.

## Run commands without a prompt

`-e` runs one startup command and may be repeated. `-c` reads commands from a
file. `--batch` exits after those commands instead of opening the REPL:

```sh
uscope --batch \
  -e 'break parse' \
  -e run \
  -e bt \
  ./parser -- input.txt
```

Batch mode returns a failing status when a command fails.
