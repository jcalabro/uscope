# Architecture

uscope separates target-specific facts from its public debugger model. Linux,
x86-64, ELF, DWARF, ptrace, and native registers stay at the edges. Clients
talk to the same model whether they are the terminal, browser, DAP server, or a
future interface.

## Debugger clients

`DebuggerHandle` is the client boundary. A client sends requests through a
bounded channel and receives acknowledgements and events. Published snapshots
are immutable. Clients do not borrow or lock mutable debugger state.

Run-control requests are acknowledged before the stop they eventually cause.
A client must use the `StopId` from the state it read when it makes a mutation.
The debugger rejects a stale ID instead of applying an action to a later stop.

Presentation shared by clients belongs in `src/present`. Do not duplicate
formatting rules in the CLI, DAP server, or browser.

## Addresses and modules

`ModuleImage` contains immutable metadata read from a program or debug file.
`LoadedModule` binds that image to one mapping in one process. An image can be
shared by several mappings and stored in the image cache.

`ImageAddress` is an address relative to an image. `VirtualAddress` is an
address in a process. Keep the types separate and convert only where a mapping
is available.

The module path, identifier, mapping, and separate debug-file location belong
to the binding. They are not properties of the image bytes. A cache key names
the input bytes and the loader sources that interpret them.

## Expressions

All source languages use one expression language. `src/eval` is pure: it sees
a program only through traits implemented by the debugger. Parsing needs no
program. Binding resolves names and target types for one frame. Evaluation
reads one validated stop.

Integer arithmetic is exact. Bit operations preserve operand width. Only a
cast truncates. Target data layout comes from debug-information providers; the
evaluator does not calculate field offsets or ABI rules.

Every example in [`docs/expressions.md`](../docs/expressions.md) runs as a
test. Change the reference and implementation together.

## Views

Views turn stored representations into logical values. Parsing and matching
live in `src/view`; built-in files live under `views`. A failed check or read
produces an explicit problem and falls back to the stored value.

View examples and tutorials are executable tests. See
[`docs/views.md`](../docs/views.md) and
[`docs/writing-views.md`](../docs/writing-views.md).

## Runtimes

Language runtimes are exposed through neutral tasks, logical stacks, code
roles, and resume points. Go and async Rust providers may discover those facts
differently, but clients must not depend on a runtime's private layout.

The runtime model describes the facts available at one stop. It must not
retain pointers into mutable debugger state. Missing runtime facts produce
typed partial results.
