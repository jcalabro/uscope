# Languages

uscope uses DWARF and native process state to debug programs written in C,
C++, Rust, Go, Zig, Odin, Fortran, D, Nim, and Ada. The core debugger behavior
is shared across languages. Runtime support adds logical tasks and
language-specific presentation where the program provides enough information.

## C and C++

Source stepping, native stacks, variables, expressions, and standard-library
views are supported. Built-in views cover common libstdc++ and libc++ strings,
sequences, maps, pointers, variants, and related containers.

Optimized C++ often has inlined calls, split variables, and template names that
depend on compiler output. uscope reports unavailable locations instead of
reconstructing values without evidence.

## Rust

Rust programs get native debugging plus views for common standard-library
types. Enums, slices, strings, vectors, maps, smart pointers, and other
containers are presented as logical values where their debug information and
views permit it.

### Async Rust and tokio

uscope can discover tokio runtimes and show their tasks. An async backtrace
follows the chain of suspended futures rather than only the native worker
thread. Stepping follows the selected task across an `.await` when the runtime
and debug information expose its continuation.

Task state is a snapshot of one debugger stop. A task changing while the
program runs is read again at the next stop.

Runtime internals and compiler output change across versions. When uscope
cannot identify a task or continuation safely, it leaves that part unavailable
instead of guessing.

## Go

uscope reads Go runtime metadata and presents goroutines as tasks. A running
goroutine is associated with its native thread; a parked goroutine can be
unwound from its saved context. Stacks can cross Go stack growth boundaries.

Stepping belongs to the selected goroutine, so work performed by another
goroutine does not complete its plan. Panics and fatal runtime failures stop at
the relevant program state when the runtime reports enough information.

Go maps, slices, strings, interfaces, channels, and common runtime-backed
values use built-in views. Compiler optimization and runtime-version changes
can make individual variables or goroutines unavailable.

## Zig, Odin, D, Nim, Ada, and Fortran

These languages use the shared native debugger and expression engine. uscope
normalizes their DWARF types and names where needed and includes built-in views
for common runtime or standard-library values where maintained.

Runtime-specific task scheduling is currently limited to Go and tokio. Other
languages appear as native threads and frames.

## Debug information decides what is available

Language support does not mean every value survives optimization. Compilers
may omit a variable, reuse its storage, describe only part of its lifetime, or
emit no unwind rule for a frame. uscope shows the available part and the reason
for a gap.

See [Known limits](reference/limitations.md) for host and format support, and
[Troubleshooting](reference/troubleshooting.md) when source, symbols, or values
are missing.
