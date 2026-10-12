# Runtime models

uscope presents language-level work as tasks and logical stacks without making
the public debugger depend on one runtime. Go goroutines and tokio tasks use
the same public concepts even though their metadata, scheduling, stack shape,
and saved state differ.

## Boundary

Runtime support has two inputs:

- Static facts loaded from module bytes: code roles, coroutine descriptions,
  resume points, function relationships, and runtime symbols.
- Dynamic facts read at one stop: runtime instances, task catalogs, task
  state, active-thread relationships, saved contexts, and await chains.

Static facts belong in `ModuleImage`. Dynamic discovery belongs in
`src/runtime_model` and the Linux runtime edge. Clients receive neutral
`RuntimeId`, `TaskId`, `TaskState`, `TaskLocation`, `CoroutineInfo`, and logical
frame records.

Do not expose a Go `g` pointer, tokio task header address, compiler generator
variant, or runtime-private enum as the identity clients must retain. Those are
evidence used to construct a public identity for one session and stop.

## Terms

A **native thread** is an operating-system tracee with registers and a native
stack.

A **task** is a unit of language runtime work: a goroutine or async task. It
may be running on a native thread, parked with saved state, runnable but not
active, or in a runtime-specific terminal state.

A **coroutine** is compiler-generated state that can resume at several source
locations. An async task can contain a chain of coroutines awaiting one
another.

A **runtime instance** is one scheduler or runtime found in the program. A
process may contain more than one supported instance.

A **logical frame** represents source execution in a task even when no native
machine frame currently exists for it.

## Stop-scoped snapshots

Runtime state is read only at a validated stop. A task catalog and every
relationship within it belong to that `StopId`. Do not carry task pointers,
saved registers, or await links into a later stop.

Task enumeration is paginated and deterministic. The same task must not appear
twice in one snapshot, and a cycle or malformed link must end with a typed
problem rather than loop. Bound memory reads, task count, stack depth, and
runtime instances.

An active task is linked to the native thread that currently runs it. That
relationship can be unknown. Idle scheduler threads have no current task.
Parked tasks have no active native thread.

## Logical stacks

A logical stack combines proven frames from one or more sources:

- native unwind state for a running task;
- saved registers or stack context for a parked stackful task;
- compiler coroutine state and resume points for a suspended stackless task;
- explicit transitions between runtime and user stacks.

Each frame records its kind and evidence. Do not make a logical frame look
native if it has no machine register context. Commands that require registers,
instruction stepping, or arbitrary memory locations must reject a context that
cannot supply them.

Logical unwinding keeps established frames when a later transition fails. The
termination says whether state was unreadable, metadata was absent, a runtime
layout was unsupported, a cycle was found, or a configured bound was reached.

## Code roles

Providers classify known functions and address ranges by role: user code,
runtime scheduling, stack growth, panic machinery, coroutine polling, and
other transitions that affect stepping or unwinding. Generic run control uses
the role, not a hard-coded runtime symbol.

Code roles are evidence, not broad name filters in clients. Add a role when a
generic algorithm needs to make a stable distinction. Keep version-specific
symbol matching in the provider.

The session setting for entering or skipping runtime code applies to these
roles. Skipping must not hide a user function merely because its name happens
to resemble a runtime helper.

## Resume points

Compiler metadata maps a coroutine state to the code location where it can
resume. A resume point is image-relative static data. The runtime model reads
the current state and binds it to a loaded module.

Several states may share a resume location, and a state can be terminal or
otherwise have no resume point. Treat a missing or ambiguous mapping as
unavailable. Do not pick the nearest line or address.

Stepping across an await uses the selected task, coroutine identity, current
activation, and proven resume sites. The execution plan remains owned by that
task. Another task reaching the same user code does not complete it.

## Go

Go support combines pclntab, DWARF, known runtime symbols, and live runtime
structures.

The runtime model locates goroutines, assigns task state, and determines which
goroutine each native thread runs. A parked goroutine exposes a saved scheduler
context. A running goroutine begins with the native thread's registers.

Go stacks can grow and move. Unwinding recognizes stack-transition functions
and switches between goroutine and system stacks only with the required saved
context. A stack boundary is not permission to scan memory for a plausible
return address.

The top of a goroutine stack can contain runtime frames that have no useful
source-level meaning. Frame presentation and stepping use code roles to skip
them under the default policy while preserving them when runtime entry is
requested.

Panic and fatal-error handling uses runtime stop evidence to publish neutral
language exceptions. Keep raised, uncaught, and terminal distinctions where
the runtime can prove them.

Go runtime layouts change. Version dispatch belongs in the provider, with a
fixture for each supported family and an explicit unsupported result for an
unknown one.

## Rust coroutines and tokio

Rust async functions compile to state-machine values. DWARF describes their
captured fields and compiler-generated variants. Static loading identifies
coroutine functions, relationships, and resume points without depending on
tokio.

tokio support then discovers runtimes and task storage, reads task state, and
finds each task's root future. Walking nested future fields reconstructs the
await chain. The walker follows type and layout facts; it does not scan the
heap for values that look like futures.

A task can be running, scheduled, idle, completed, or in a state the current
runtime provider does not understand. A running task can join its logical
future chain to a native worker stack. A parked task may have only its future
chain and no native registers.

`tokio_unstable` can provide extra facts such as spawn locations. Treat those
as optional enrichment. Core task identity, state, and stack behavior must
state clearly when they require unstable runtime instrumentation.

Pinned and boxed futures, trait-object erasure, combinators, and compiler
layout changes are common sources of partial chains. Each traversal step needs
a concrete type and storage relationship. Stop where proof ends.

## Expressions and values in tasks

Selecting a logical frame changes name binding to that frame's source scope.
Variables stored in a coroutine are read through the compiler-described future
value. Variables that exist only in a native activation use native frame
locations.

`$task` is the selected task ID or the task running on the selected thread. It
is unavailable when no task relationship is known. `$future` is available in
a frame whose coroutine value is known.

Views apply after a task variable has been located and typed. Runtime support
must not bypass the evaluator or synthesize container layouts in client code.

## Run control

Task-aware stepping adds constraints to an ordinary execution plan:

- The selected task owns progress.
- Runtime scheduling and poll frames may be skipped according to policy.
- A suspension can move the task to another native thread.
- Entry breakpoints may be reached by unrelated tasks.
- A task can complete, panic, or disappear before reaching a planned site.

At each candidate stop, identify the current task before deciding whether the
plan completed. If identity is unavailable, return a clear stop or plan
failure; do not let an unrelated thread satisfy it.

Watchpoints on task locals have a similar lifetime problem. The variable's
storage can move when a Go stack grows or a future moves before it is pinned.
Keep the watch scope and invalidation reason explicit.

## Adding another runtime

Start with the neutral behavior the user needs:

1. How are runtime instances found?
2. What is one task's stable identity during a stop?
3. How are tasks enumerated with a hard bound?
4. How is a running task associated with a native thread?
5. What saved state exists for a parked task?
6. How are logical frames proven and variables located?
7. Which code roles affect stepping and exceptions?
8. How do version and build differences fail visibly?

Implement static facts at the debug-information edge and dynamic facts through
the runtime traits. Reuse public tasks, frames, states, and exception records.
Add a public concept only when the new runtime has a user-visible fact that
cannot be represented honestly.

## Testing runtime support

Runtime tests need synchronized programs whose task topology is known. A
fixture should expose barriers for creation, running, parking, waking,
migration, panic, and exit. The test waits for those barriers; it does not
sleep and hope the scheduler reached one.

Check invariants at every relevant stop:

- task IDs are unique and deterministic within the snapshot;
- active task and native thread relationships agree;
- logical stacks end at the expected root or state a reason;
- frame variables come from the selected activation;
- stepping follows one task across scheduling;
- task completion and runtime shutdown clean every plan and cache;
- unsupported versions fail as unsupported, not as empty task lists;
- work stays within task, read, depth, and time bounds.

Use a version matrix only where runtime or compiler output changes behavior.
Run runtime integration tests, DAP and web task tests, stress, soak where
applicable, and the long simulator sweep for run-control changes.

The detailed historical rationale and producer facts remain in
`plans/go.md` and `plans/tokio.md`.
