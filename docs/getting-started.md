# Getting started

This page takes you from a program with debug information to a stopped uscope
session.

## Install uscope

> TODO: uscope does not have a supported installation method or stable release
> artifact yet. Add installation instructions after the project publishes one.

The repository contains a pinned environment for contributors. It is not an
end-user installation method.

## Build a program for debugging

Compile the program with debug information. Optimized programs can be
debugged, but their variables and control flow may no longer match the source
exactly. Start without optimization when learning the tool.

For example:

```sh
cc -g -O0 -o hello hello.c
```

uscope reads DWARF debug information from the executable or from a separate
debug file. A stripped program without available debug information can still
be inspected by address and instruction, but source lines, types, and local
variables may be missing.

## Start a terminal session

Launch a program by passing its executable and arguments:

```sh
uscope ./hello -- first second
```

uscope loads the program and opens a prompt:

```text
(uscope)
```

Set a breakpoint and run:

```text
break main
run
```

At a stop, these commands cover the basic workflow:

```text
list                show source around the current line
bt                  show the stack
locals              show local variables
print name          evaluate an expression
next                step over a source line
step                step into a call
continue            run until the next stop
help                list commands
quit                end the session
```

See [Debugging](debugging.md) for launch, attach, core-dump, breakpoint, and
stepping workflows. The [command-line reference](reference/command-line.md)
lists exact invocation forms and REPL commands.

## Use the browser or an editor

Start the browser interface with:

```sh
uscope web ./hello
```

See [Browser](browser.md) for its views and sharing model.

uscope also speaks the Debug Adapter Protocol. See [Editors](editors.md) for
client setup and supported behavior.
