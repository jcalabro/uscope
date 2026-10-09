# How uscope debugs tokio

tokio is the second runtime uscope understands, after Go's, and the first
whose units of work have no stacks. A tokio task is a future in a heap
cell: an async function compiled to a state machine, holding the futures
it awaits, polled by whichever worker the scheduler picks. Most of the time
a task runs on no thread at all. This document explains how uscope handles
that: where the knowledge lives, the decisions behind it, and the traps
found on the way. It is for people changing uscope, and ends with the
walkthrough a person runs to check the whole experience.

What a user sees is in [cli.md](cli.md) and [dap.md](dap.md). The design
record, with the experiments and the survey of other debuggers behind these
decisions, is [plans/tokio.md](../plans/tokio.md). Go's counterpart is
[go.md](go.md), whose seams this work reused.

Support covers the pinned toolchain from `flake.nix` and tokio 1.52.3 on
Linux x86-64: unoptimized and optimized builds, the multi-thread and
current-thread runtimes, `LocalSet`s, several runtimes in one process,
blocking pools, `tokio_unstable` spawn locations, tokio's `parking_lot`
feature, legacy symbol mangling, attaching, and core dumps. Another
tokio release is read the same way wherever its debug information binds,
and every list of its tasks says it is unverified.

## What async Rust and tokio demand

- **Tasks have no stacks.** A suspended task is the chain of futures its
  root future awaits, each a coroutine whose state says where it waits.
  Its "backtrace" is read from heap memory, not unwound from registers.
- **Every async body has the same name.** rustc names each resume function
  `{async_fn#0}` or `{async_block#0}` and each future type
  `{async_fn_env#0}`, and describes a coroutine's states as a DWARF variant
  part keyed on `__state`. The variant members carry the await's line.
- **Each codegen unit describes things anew.** An optimized build has
  several out-of-line copies of one coroutine's type, its functions, and its
  drop glue, one per unit that uses them. Types are compared by identity,
  never by DIE.
- **Optimized builds inline bodies.** At O3 an async function's body is
  often inlined into its awaiter's, into a combinator's, or into tokio's
  `raw::poll::<T, S>`, and no function of its own remains.
- **No list names every runtime.** A runtime is found through the threads
  that entered it: each thread's `CONTEXT` thread-local holds its runtime's
  handle, its scheduler when it is a worker, and the task it polls.
- **A task moves.** A worker steals it, a wake from outside the runtime
  sends it through the inject queue, and `block_in_place` hands a worker's
  core to a new thread mid-poll.
- **The conventions are not in DWARF.** Layouts are, by name, in every
  build. The task state's bits, the meaning of `Stage`'s variants, and the
  panic payload's shape are tokio's and std's conventions, pinned by tests.
- **Time keeps running.** A program stopped for a minute finds its timeouts
  expired. uscope never changes the program to prevent it.

## Where tokio lives

Most of uscope does not know tokio exists:

| Place | What it knows |
|---|---|
| `src/debug_info/coroutines.rs`, `src/debug_info/roles.rs` | rustc's coroutine DWARF, normalized to `CoroutineInfo`; resume points; code roles for tokio, `mio`, `core::future` glue, and std's panic machinery |
| `src/runtime_model/futures.rs` | The future walker: a chain of awaits through coroutines, `Box`, `Pin`, trait objects, and single-coroutine records. It names no runtime |
| `src/runtime_model/tokio` | The runtime at one stop: runtimes, tasks, threads, futures, local sets, task starters |
| `src/runtime_model/rust` | std's panics, through the `__rustc::rust_panic` hook |
| `views/tokio.views` | tokio's types: handles, locks, channels, `Notify`, timers, `JoinSet`, sockets and the futures that read and write them |

Everything else speaks of tasks, suspended futures, async frames, stack
segments, code roles, and language exceptions. `runtime_model_stays_pure`
keeps process control and I/O out of the runtime model, and
`languages_stay_at_their_seams` keeps tokio's names out of the backend, the
clients, the evaluator, and the protocol.

The rule from Go holds. **A fact true of every execution of a function is
static, and belongs to the debug-info provider:** a function is a coroutine's
resume function, its state dispatch is here, it is tokio's. **A fact about
one stop is dynamic, and belongs to the runtime model:** which task a thread
polls, where a suspended future waits.

## Coroutines

The provider reads each coroutine type once, at load:

- its state member and states, each `Unresumed`, `Returned`, `Panicked`, or
  suspended at an await with the await's line and the member it awaits;
- its captures, the members present in every state;
- the functions that run it, and per function its resume points: where its
  dispatch sends each state, decoded from the jump table an unoptimized
  build has, or unknown with a reason. A resume point is verified against
  hardware by a test that stops there.

Async functions are named for the source: `async leaf`, with the
constructor that returns the future a `Wrapper`, and `break f` binding the
body. An await line's breakpoint binds where the line is reached, never the
dispatch that resumes there, so it fires once per arrival. No line binds
drop glue.

## The runtime model at one stop

**Runtimes.** The model reads `CONTEXT` on every thread: its eager storage
must be alive, its handle's variant gives the flavor, and handles are
grouped by address. A runtime no thread has entered cannot be found, which
is a stated limit, not a gap. A `LocalSet` is found where a thread's
`CURRENT` names it while running it, and where a thread drives a future
that holds it, as `LocalSet::block_on` and `run_until` do. An optimized
build may lose that future; the list then says which thread drives a future
that may run a set whose tasks it cannot list.

**Tasks.** Each runtime's `OwnedTasks` is a set of shards, linked lists
through each task's trailer. Every node is checked before it is listed: its
list id is the list's, its vtable polls through `raw::poll`, and it links
back to the node before it. A shard whose lock is held may be mid-change; its
tasks are listed with that gap. A damaged shard is read up to the damage,
which the gap names, and every other shard is listed. Pages resume from a
node, not an index, so a page costs a bounded read per task wherever it
begins; a list's count is checked only when one page reads the whole of it.
The blocking pools' queued closures come from each pool's ring, and running
ones from their threads.

**States.** `RUNNING` is `Running`, on the thread whose `CONTEXT` names the
task; with no such thread the state is unknown, with the reason.
`NOTIFIED` is `Runnable`. Neither is `Blocked`, with what the leaf future
waits for as its detail, from the leaf's view. `COMPLETE` is `Exited`.
A worker's launch is the runtime's own task, hidden unless asked for.

**Threads.**

- A worker whose `CONTEXT` names a task on its runtime's list runs that
  task; one polling its own launch is idle.
- A pool thread runs its closure's task, or waits idle for one.
- A thread that entered a runtime without a scheduler blocks on it, as
  `block_on` does, and is the program's own. So is a worker still starting,
  which enters its runtime before it sets its scheduler.
- Any other thread is not the runtime's.

**The future.** A task's vtable `poll` is `raw::poll::<T, S>`, whose DIE
names `T` and `S`; `Cell<T, S>` gives where the stage is, and `Running`
holds the future. With `tokio_unstable`, the vtable's spawn location offset
gives where the task was spawned.

## Async backtraces

A suspended task's stack is the walker's chain: the leaf future it waits
on, then each async function at its await, out to the one the task began in.
An async frame has no registers; its variables are the members its state
keeps, read relative to the future, and `$future` is the future. A running
task's frames are its thread's, down to tokio's `Dispatch` frame, which hands
the thread to the task. A thread blocking in `block_on` shows the future it
drives just before the frame that drives it.

Backtraces fold the runtime's frames, Go's too: a run of runtime frames is
one line, and `bt -r` shows them all. Frame numbers count every frame.

## Run control across awaits

A step belongs to the task it began in. `next` over an await that returns
`Pending` lets the task go on and waits where its future resumes: plan
breakpoints at the future's resume points, taken as the step's own when the
body there names the same future, by address and an equivalent type. Other
tasks running the same function meanwhile pass. Where optimization inlined
the body, the step waits at the body's statements and at the other lines of
the async functions around it, less the state dispatch.

A step watches how its task may end instead of resuming: every copy of its
future's drop glue, the runtime's code that takes up the task again, the
dispatch's return, and the glue that frees the task's cell. It then says the
future was dropped or the task finished or was cancelled, never that it
stepped.

`step task` on a line that spawns follows the new task. A runtime names its
task starters: Go's `newproc1`, which names its goroutine once it returns,
and tokio's `OwnedTasks::bind_inner`, the part of `bind` the same for every
future, and a local set's `Shared::schedule`, which name the task as they
begin. `schedule` also schedules woken tasks: a task given to it whose root
coroutine has not begun is a new one. The step then waits where the task's
root coroutine begins, or, where that body is inlined, at its statements,
and ends at the first the new task runs.

**Which task a thread runs, at a hit.** Run control decides whether a hit
is a step's, or evaluates a `$task` condition, as the thread hits, before
the other threads stop. Reading the runtime's list then races with threads
still changing it. A runtime model therefore answers what the thread's own
state names, `current_task`: for tokio, the id its `CONTEXT` holds. A
published stop still describes threads by their full activity.

## Panics

Every Rust panic stops as it begins, through `__rustc::rust_panic`, which
std calls after the panic hook formats the message into the payload and
before any `catch_unwind` decides anything. std ships no private types, so
the payload's type is found by symbolizing its vtable. The stop selects the
program's frame below std's and core's panic machinery, and names the task
the thread runs, which tokio will turn into a `JoinError`.

## Traps found on the way

- An optimized `CurrentThread::block_on` keeps a moved-from copy of its
  future, which reads as `Unresumed`; only pinned variables are trusted.
- `select!` without `biased` makes LLVM duplicate branches, a copy of which
  no line breakpoint binds, in gdb as in uscope.
- A coroutine's state is written only as it suspends; a running coroutine's
  state is stale, and only `Returned` is reliable mid-poll.
- tokio frees a task's cell in inlined code; a step watches the glue for
  `Box<Cell<T, S>>`, never reads a task's state after its poll returns.
- A blocking task has no header while it runs, so it has no spawn location.
- Every async env type now parses as a type name, so resolving a type
  argument that names one compared thousands of candidates; resolutions are
  kept for the load.
- glibc 2.42 guards thread stacks with `MADV_GUARD_INSTALL`, which `gcore`
  saves as zeros; the fixtures remove the guards before dumping a core.
- Adding a fixture crate changes its lockfile, which rebuilds the vendored
  crates and every tokio fixture.
- tokio's own `Mutex` wraps std's, or with its `parking_lot` feature, which
  `full` turns on, parking_lot's beside a `PhantomData` of std's. Each
  shard's and each blocking pool's lock is bound as whichever the program
  has: std's futex word is held while nonzero, parking_lot's byte while its
  low bit is set.

## Testing

The fixtures are real programs in the Nix-vendored workspace
`tests/fixtures/rust/tokio`, built unoptimized and optimized, each forcing
the situation it exists for and printing `TRUTH` lines from the `truth`
crate: each task's id, the awaits it holds, its thread, and its values.
Checkpoints wait until nothing moves, by the runtime's park counts and
`/proc`, never by sleeping.

| Fixture | What it forces |
|---|---|
| `std-async` | Async functions under a hand-written executor, with no tokio |
| `workers` | Eight tasks parked at different awaits; also built line-tables-only, stripped, remapped, unstable, with parking_lot's locks, and legacy-mangled |
| `steps` | Siblings stepping through the same functions across pending awaits, on each runtime, and a task that spawns another |
| `cancel`, `panics` | Every way a task's future ends early, and every kind of panic |
| `drivers`, `runtimes` | `block_on` in each form; two runtimes and two local sets in one process |
| `shapes`, `values` | Every shape of await chain; every viewed type in every state, sockets and their futures among them |
| `server`, `scale` | The attach and soak target, a listener and a task per connection; a hundred thousand tasks |
| `blocking`, `migrate`, `deadlock`, `corrupt` | A blocking pool's queue; a task resuming elsewhere during `block_in_place`; a mutex cycle; a list the program damages |

`tests/tokio/invariants.rs` checks every stop of most scenarios: no task
listed twice, each running task on the thread whose activity names it, each
thread's frames consistent with what it runs. The model's list walk has
property tests on damaged memory bound to a real image's layout, and the
future walker never panics or loops on any memory. `just stress` runs the
suite under load, and `just soak MINUTES` attaches to the server under
steady load.

## Walkthrough

A person runs this at the end of a phase that changes tokio support, against
the `server` fixture, in a terminal and in VS Code. Anything that surprises
them becomes a test or a change to these docs.

1. Start the server, `build/test-programs/tokio-server-o0`, and connect two
   clients with `nc` to the address it prints.
2. Attach: `uscope --attach PID`. `tasks` lists the listener's task,
   `waiting until readable`, and one task per connection, `reading a line
   from fd N`; `tasks -g` groups them. A client that sends `fd` learns
   which N is its own.
3. Find a stuck request's task: `task N` and `bt` show its handler's await,
   and the leaf says what it waits for. `print` lists what the handler keeps
   across the await.
4. Step through a handler: `break answer`, `continue`, send a line from a
   client, then `finish` and `next` past the `write_all` await. Each step
   stays in the same task, whichever worker runs it. A `next` over
   `next_line().await` waits until that client sends another line.
5. Catch a panic: with the `panics` fixture, `run`, and see the stop name
   the message, the program's frame, and the task.
6. Detach with `quit`; both clients still get answers, and `quit` from a
   client ends the server.

In VS Code, the same: tasks are threads, a suspended task's async frames
expand with their variables, `next` crosses a pending await, and a panic
stops with its message in the exception widget.
