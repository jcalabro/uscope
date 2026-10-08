# tokio: plan

Status: proposed, 2026-10-07. Decisions marked *(settled)* were made with
the maintainer. Like Go's, this plan keeps tokio support as simple as it can
be while staying rigorous, and defers anything that is not needed for that.

Debugging a tokio program in uscope should feel like debugging ordinary Rust.
That means:

- A task is something you can list, select, and inspect, as a goroutine is.
- A suspended task shows a backtrace: the chain of `async fn`s it is awaiting
  in, each at the line of its `.await`, with the variables that live across
  that await.
- `next` over an `.await` ends on the next line of the same task, whichever
  worker thread polls it next.
- `step` into `f().await` lands in `f`'s body.
- `finish` from an `async fn` returns to the line that awaited it, with the
  value it produced.
- A panic in a task stops where it happened, even though tokio will catch it.
- Everything the runtime does to make this work stays out of the way unless
  asked for.

This record sets out:

- where uscope stands;
- what tokio and rustc's async lowering demand;
- what the prior art does;
- the decisions that follow and the architecture they need;
- the order of the work and how it is tested.

It closes the TODO item "First-class tokio support". It builds on the seams
`plans/go.md` built for Go, and changes them only where a runtime whose tasks
have no stacks needs it.

## Where uscope stands

**What works today.** These were run on 2026-10-07 against tokio 1.52.3,
compiled by the pinned rustc (1.99 nightly, 2026-07-10). Each build was
debug (`opt-level=0`, `debuginfo=2`), release with `debuginfo=2`, or
`panic="abort"`. What works:

- Line breakpoints in async bodies (except on `.await` lines), in
  `spawn_blocking` closures, and in ordinary functions.
- Complete unwinding through tokio, std, and libc, with correct inline frames
  in release builds.
- In debug builds, locals saved in the future across an await read correctly,
  even after the task moves to another thread.
- `next` over an await that is ready at once, and `finish` from a poll that
  completes.
- Attaching to a tokio server and detaching from it; the server keeps
  serving.
- `print/r` on tokio's harness frame shows the whole await tree by hand.
  `harness.cell.pointer.core.task_id` gives the task's id. The information
  is all there; nothing presents it.

**What failed in experiments.**

- **Tasks are invisible.**
  - `tasks`, `task`, and `$task` say "the program has no tasks", and
    `step task` silently behaves as `next`.
  - Attached to a server with 7 parked tasks, uscope showed 26 threads, all
    named `tokio-rt-worker`, in `futex_wait` or `epoll_wait`. The only frame
    of the program's own code on any of them was inside a `spawn_blocking`
    closure.
- **Every async body has the same name.** Every resume function shows as
  `{async_fn#0}`, and `#[tokio::main]`'s body as `{async_block#0}`, without
  the `workers::leaf` that qualifies it.
  - A stop in `leaf` shows three frames all called `{async_fn#0}`.
  - `break workers::leaf::{async_fn#0}` is refused, though `info calls`
    prints that name.
- **`break my_async_fn` stops in the wrong function.** It binds the function
  that only *builds* the future, so it stops before the body ever runs and
  never inside it.
  - `finish` from there prints `returned ({async_fn_env#0}) leaf =
    <unavailable: unsupported variable feature…>`.
  - In release that constructor is inlined away, and `break leaf` fails with
    "no function named 'leaf'".
  - Line breakpoints also bind a second location in the future's drop glue,
    up to 13 locations in release.
- **A breakpoint on an `.await` line fires only on resumption.** uscope binds
  the lowest-addressed row of a line. rustc places each await's resume path
  first, so the breakpoint fires when the task is re-polled after that await
  was pending, never when execution first arrives there.
  - `break server.rs:13` on a `write_all(..).await` that never pends had 0
    hits over hundreds of echoes.
  - In release, `break steps.rs:18` on a `sleep(..).await` had 0 hits over
    24 executions of the line.
- **`next` over a pending await leaves the task.**
  - The `Poll::Pending` return ends the step at the parent's await line.
  - The next `next` walks into tokio's harness (`core.rs`, `harness.rs`).
  - The same happens on the current-thread runtime and in a `LocalSet`.
  - Steps never strayed into another task in 11 runs, but only because none
    crossed a pending await.
- **`step` into `f().await` takes 8 steps** before reaching `f`'s body. It
  goes through the future's constructor, `IntoFuture::into_future`, and
  `Pin::new_unchecked` first.
- **`finish` cannot tell `Pending` from `Ready`.** Every `Poll<T>` return
  value is `<unavailable: unsupported variable feature: the source type
  representation>`.
- **Stale locals are shown as real values.** A body's copy of an argument
  lives in a stack slot that the debug information claims for the whole
  block. After the body resumes from an await, that slot holds another poll's
  leftovers, and `print id` prefers it to the correct copy saved in the
  future.
  - `break … if id == 3` once stopped with impossible values (`id = 3,
    doubled = 13`), and in another run never stopped at all.
  - A `for step in 0..3` loop variable showed `step = 8`.
- **Locals listings are noisy.**
  - Every argument appears twice, as the saved copy and as the body's copy.
  - Two `<anonymous parameter> = <malformed: parameter has no name>` entries
    and `_task_context` appear at every stop.
  - The future itself (`self`) cannot be reached by name.
- **Backtraces are mostly tokio.**
  - A typical stop has 65 frames, 3 of them the program's.
  - Each worker thread is itself a blocking-pool task, so the harness and
    `catch_unwind` chain appears twice.
  - tokio's frame names run past 300 characters of generic arguments.
  - In release, merged identical functions name `current_thread::Handle` on
    a multi-thread runtime.
- **Simultaneous breakpoint hits are lost from view.** When several workers
  hit a breakpoint at once, uscope makes one stop. The other hits show only
  in `threads` (`stopped at breakpoint 2 (hit 1)`), and no later stop
  reports them. Two of three runs lost a `spawn_blocking` breakpoint this
  way. This is existing all-stop policy, but tokio meets it constantly: four
  workers hit the same line together.
- **Task panics never stop.**
  - tokio's `catch_unwind` turns a task's panic into a `JoinError`, and a
    panic in `main` exits with status 101 without stopping.
  - With `panic="abort"` the program stops as SIGABRT, with libc's frame
    selected and the panicking frame 16 frames down.
  - `break rust_panic` works around it. The program's frame is then #7.
- **Values.**
  - A future's state prints as `3 {…}` rather than `Suspend0 {…}`. rustc
    names the state on the variant's type, and uscope prints the member
    name.
  - `ptype` shows only `{async_fn_env#0}`, which cannot be written in an
    expression.
  - A `JoinHandle` shows only a raw pointer: no task id, state, or output.
  - `Arc` prints `{…}` even with `pp`, and `*arc` and `*guard` are refused.
  - `UnsafeCell({…})` hides a tokio `Mutex`'s contents, channels, and task
    stages.
  - tokio's `Instant` prints `{std = 94h2m41s}`.
  - `print/x` on an `Atomic<usize>` printed decimal.
  - Listing one tokio frame's locals failed with "variant part has neither a
    discriminator nor a tag type".
- **Optimized locals are mostly gone.**
  - Locals of an inlined async function are missing entirely ("no variable
    is named `mine`").
  - The parent's saved locals show as `optimized out`, though they are in
    memory.
- **Loading is slow.** A debug uscope spends 10–12 s of CPU loading a 20 MB
  tokio binary; a release uscope takes 1.8 s and 230 MB. This is the
  existing TODO item "Loading binaries and debug info performance
  improvements"; tokio programs make it pressing.

**What the code says.** These were found by reading the code:

- No code knows about coroutines, `__awaitee`, tokio, or Rust panics.
- Rust code has no code roles beyond `_start` and `__restore_rt`.
- Every Rust fixture is built `-C panic=abort`, and most are `no_std`.
- No fixture depends on a crate.
- The runtime seams `plans/go.md` built assume a task with a stack:
  - `TaskContext` is either a thread or saved registers.
  - A task-owned step needs `StackSegment::Task` and stack bounds.
  - `Activation` has no depth that is not a stack address.
  - `RuntimeId` is the module's id, so a module carries at most one runtime.
- `runtime_model_stays_pure` forbids the substring `tokio` in
  `src/runtime_model`, so a tokio model would fail it as written.

## What tokio and async Rust demand

- **A task is a heap object, not a stack.**
  - `tokio::spawn` allocates a `Cell<T, S>`, `#[repr(C)]`, holding a
    `Header`, a `Core` (scheduler, task id, and the future's `Stage`), and a
    `Trailer`.
  - The `Header`'s address is the task's identity. Wakers, `JoinHandle`s, and
    run queues all point at it.
  - Between polls, a task has no thread, no registers, and no frames. Its
    whole state is the future: a state machine rustc generated.
- **A future's state is a nest of state machines.**
  - Every `async fn` and `async` block is a coroutine: an enum-like value
    whose discriminant is an artificial `__state`.
    - State 0 is `Unresumed`, 1 `Returned`, and 2 `Panicked`.
    - States from 3 on are `Suspend0`, `Suspend1`, and so on, one per
      `.await`.
  - Each suspend state holds:
    - the locals live across that await;
    - the awaited future itself, in `__awaitee`, stored inline;
    - the captured arguments.
  - The awaitee may be another coroutine, a library future (`Sleep`,
    `Recv`, `JoinHandle`), or a boxed `dyn Future`.
  - Following the awaitees from a task's root future gives its logical call
    stack.
- **The chain is only true at rest.** A task being polled has its live state
  in the polling thread's registers and stack. Its saved discriminants are
  the states it resumed from (or `Unresumed`), not where it is now.
  - A running task is read from its thread's frames.
  - A suspended task is read from its future.
  - The two are never mixed.
- **M:N scheduling with work stealing.**
  - A multi-thread runtime has worker threads plus a blocking pool, and
    workers steal each other's tasks.
  - A task resumes on whichever worker polls it next. Each resume is a fresh
    call of the root future's `poll`, which re-enters every coroutine down
    the chain to the await that suspended.
  - A current-thread runtime runs everything on the thread in `block_on`.
- **`block_on`'s future is not a task.** `#[tokio::main] async fn main` is
  driven by `block_on` on the main thread. Its future is a local of
  `CachedParkThread::block_on` (or of the current-thread `CoreGuard`). It has
  no id and is on no list.
- **Not every task is on the runtime's list.**
  - `OwnedTasks` is a sharded list (4 × the next power of two of the workers,
    capped at 65536) of intrusive lists, linked through `Trailer.owned`.
  - It holds `spawn`ed tasks until they complete.
  - `spawn_blocking` tasks are in the blocking pool's queue while queued, and
    only on a thread's stack while running.
  - `LocalSet` keeps its own list.
  - A completed task whose `JoinHandle` lives is on no list.
  - Worker threads are themselves blocking-pool tasks.
- **There is no global registry of runtimes.** A runtime is reached through
  the threads in it: the `CONTEXT` thread-local holds the handle of the
  runtime a thread has entered. It also holds `current_task_id`, which tokio
  sets around every poll and every drop of a future.
- **State bits exist only as Rust constants.**
  - The bits are `RUNNING` 0x1, `COMPLETE` 0x2, `NOTIFIED` 0x4,
    `JOIN_INTEREST` 0x8, `JOIN_WAKER` 0x10, and `CANCELLED` 0x20, with the
    reference count from bit 6.
  - rustc emits no DWARF for them.
  - Every other layout is in DWARF.
- **Panics are caught.** tokio polls inside `catch_unwind`. A panicking task
  completes with a `JoinError` that is silently dropped if nobody awaits its
  handle; this is a common "my task just vanished" bug. std calls
  `__rustc::rust_panic` once the panic hook has run, as a place for debuggers
  to stop.
- **The compiler's lowering leaves traps.**
  - Each await's resume path is laid out before its first-arrival path, and
    its line rows come first.
  - The state dispatch at the resume function's entry carries the function's
    header line.
  - Stack slots outlive the poll that filled them.
  - In release builds coroutine bodies are inlined into tokio's
    `raw::poll::<T, S>`, so they have no frames of their own.
- **Large futures are boxed by tokio.** A future over 2048 bytes in debug
  builds, or 16384 in release, is spawned as `Pin<Box<F>>`.
- **Time keeps running while stopped.** Wall-clock time spent at a stop
  counts toward every `sleep`, `timeout`, and `Interval`. After a long stop,
  timers fire together.
- **tokio and rustc both move.**
  - tokio 1.53 moved `OwnedTasks` within `Shared`, changed the generic arity
    of `ShardedList` and `LinkedList`, and added a field to `Header`.
  - Two open rustc PRs (#135527 and #157309) would change coroutine
    debuginfo.
  - There is no stable debugger interface: tokio#6950 asks for one and has no
    maintainer response.

## Prior art, and where to do better

Five tools inform this design: BugStalker, hansei, Fuchsia's zxdb,
tokio-console, and tokio's own task dumps. Their code is studied, not copied:
hansei is MPL-2.0, and the rest are MIT or Apache.

**BugStalker** is the only live Rust debugger with tokio support. It offers
`async backtrace`, `async next`, and `async finish`.

- **Finding tasks.** It finds workers by matching frame names, reads
  `CONTEXT`, and walks `OwnedTasks`.
  - It probably lists only the first task of each shard: it follows
    `Header.queue_next`, which links the inject queue, not the owned list.
  - It supports only the multi-thread runtime.
- **Future types.** It finds a task's future type through the vtable's
  `raw::poll::<T, S>` and its DWARF template parameters.
- **Async step over.**
  - It plants a temporary breakpoint on every statement row of the current
    function. It keeps a hit only if `_task_context` or the thread's current
    task id matches.
  - It spends a hardware watchpoint on `Header.state` to notice that the task
    has completed.
  - Its own test shows stops on resume-dispatch rows: after a resume it lands
    on the function's header line and on earlier lines.
- **No locals or lines per async frame.** Its output reads "await point N".
- **It breaks with each compiler and tokio release.** It carries per-version
  field paths, and finds the tokio version by searching `.rodata`.

**hansei** (Oxide) is the most rigorous prior art, but it reads core dumps
only.

- **Version families.** It supports tokio 1.47–1.53 in layout families and
  refuses unknown versions.
- **Await lines.** It pairs each `SuspendN` with the `__awaitee` locals of the
  resume function, because a macro's awaits carry the expansion site. It
  leaves ambiguous pairs unmatched rather than guess.
- **Hidden tasks.** It finds tasks through `JoinHandle`s, wakers, the
  blocking queue, and `LocalSet`s.
- **What a task waits on.** It explains the leaf of each chain: a timer
  deadline, a `JoinHandle`'s task, a semaphore's permits with its queue of
  waiting tasks, or readiness on a file descriptor.
- **Running tasks.** It splices a running task's native frames above its
  committed chain and warns that the state may be torn.
- **Moved-out captures.** It marks an async block's captures uncertain in
  suspended states, since they may have been moved out.

**zxdb** shows an async backtrace from each variant's `decl_line`. It
special-cases `select!` and `join!` by file name, so its output shows
`select_mod.rs` lines.

**tokio-console** needs `tokio_unstable` and tracing instrumentation. It
shows poll and wake timing, task names, and lost-waker warnings. A debugger
reading memory cannot show those, and the console cannot show stacks,
locals, or a core dump's state.

**tokio's task dump** (`Handle::dump`) needs `tokio_unstable` and `taskdump`.
It re-polls every task in a tracing mode, which needs the runtime running.

**UX that works elsewhere:**

- **Tasks as selectable threads.** LLDB's Swift `task select` and hansei's
  task cursor both let the ordinary commands (`bt`, `up`, `down`) work on a
  task.
- **A step over an await keyed on the task's object.** Swift LLDB runs to an
  address on an async context. IntelliJ plants one breakpoint at the
  coroutine's resume entry, filtered to the same continuation, then steps to
  the next line.
- **Tasks grouped by shared stack, with counts.** Visual Studio's Parallel
  Stacks and hansei's census both do this.
- **A creation site** (Kotlin) and a **reason for each wait** (hansei).

**Where uscope can do better than all of them:**

- live processes and cores, by the same code;
- both runtime flavors, and blocking tasks;
- an exact line and the saved locals for every async frame;
- steps that stop once, on the next line, in the same task;
- every Rust panic a stop;
- every result complete or carrying its reason, never a guess;
- every layout read from DWARF by name, so a tokio release that moves a field
  costs nothing.

## Decisions

**A tokio task is a task** *(settled by the Go design)*.

- tokio's tasks are the public model's tasks, and `task::Id` is the task's
  number.
- tokio's ids come from one process-wide counter, so they are unique across
  runtimes in one process.
- A task's state, the thread it runs on, its labels, and where it resumes are
  the model's existing fields.
- tokio's noun is `task`, so `tasks` and `task N` need no aliases.

**Runtime knowledge sits in pure runtime models, beside the debug-info
provider, as for Go.**

- **`src/runtime_model/rust`, for std's runtime.** It holds the panic hook,
  the panic message convention, and nothing else. Every Rust program gets it,
  with or without tokio.
- **`src/runtime_model/tokio`, for tokio.** It holds:
  - the contract;
  - finding runtimes and listing their tasks;
  - which task a thread runs;
  - where a task's future is;
  - the driver frames whose futures are not tasks;
  - the hooks that see a task polled again and see it finish.
- **What is static goes to the provider.**
  - How rustc lowers `async` is a fact about code. The DWARF provider
    normalizes coroutine types and resume functions.
  - Which code is tokio's machinery is a code role.
- **Async backtraces are neutral.** Walking a future's chain uses only the
  provider's coroutine facts and memory. It knows no tokio and no Rust names,
  so any executor (smol, embassy, glommio) or a C++ coroutine runtime could
  later supply roots to the same walker.

**One module may carry several runtimes.**

- `detect` returns every runtime an image carries: std's and tokio's for
  most Rust programs.
- `RuntimeId` stops being the module's id and becomes an index into the
  session's bound runtimes.
- Go keeps one model per image.
- A process with several tokio runtimes still has one tokio model per image.
  It finds each runtime instance at each stop.

**Layouts come from DWARF. Only conventions are written down** *(as for
Go)*.

- **Read by name from tokio's DWARF:** `Header`, `Trailer`, `Vtable`,
  `Core`, `Stage`, `Cell<T, S>`, `OwnedTasks`, `ShardedList`, `LinkedList`,
  the multi-thread and current-thread `Handle` and `Shared`, `Context`, the
  blocking pool's `Spawner` and `Shared`, and `task::Id`.
  - Type names are matched loosely, by path and arity-insensitively, since
    1.53 changed the arity.
  - Generic instances are found through the vtable's `poll::<T, S>`, never by
    building a type-name string.
- **Written down, each with the versions that use it:**
  - the state bits;
  - the path from `CONTEXT` to `OwnedTasks`;
  - that owned lists link `Header`s through `Trailer.owned`;
  - that `Stage::Running` holds the future;
  - where the blocking pool queues;
  - which functions drive a non-task future (`block_on`);
  - that a running task's saved state is stale.
- **A runtime contract lists every name.**
  - A missing name makes only the feature that needs it unavailable, with a
    reason naming the field.
  - A test checks the contract against the pinned tokio, as
    `a_missing_name_makes_only_what_needs_it_unavailable` does for Go.

**Support one pinned tokio** *(settled)*.

- The fixture workspace's lockfile pins tokio 1.52.3, the release uscope
  itself locks today.
- Moving the pin is a change of its own: contract, conventions, views, and
  fixture expectations move together.
- The version comes from tokio's compile-unit path (`…/tokio-1.52.3/src/
  lib.rs`).
  - A path remapped out of recognition gives "tokio's version is unknown".
    The model then reads what binds and flags every result unverified.
- Another tokio release is read wherever the contract binds against its DWARF,
  and every result carries the gap "tokio 1.X is unverified".
- **The rustc pin.** The coroutine conventions (variant naming, `__awaitee`,
  `decl_line` on variants) are verified against the pinned nightly in
  `flake.nix`. A producer from another rustc is read the same way and flagged
  unverified.

**Fixtures get their crates from a Nix-vendored lockfile** *(settled)*.

- `tests/fixtures/rust/tokio/` is a cargo workspace with its own `Cargo.lock`.
- `flake.nix` vendors exactly those crates as a fixed-output derivation, in
  the manner of `importCargoLock`.
- `build-test-programs.sh` builds with `cargo build --offline --locked`
  against that vendor directory.
- Nothing is fetched at build time, and nothing third-party is checked in.
- The Rust tests launch the fixtures but never invoke cargo, as AGENTS.md
  requires.

**Async bodies are the functions users wrote.** The provider normalizes the
compiler's split of an `async fn`:

- **The resume function presents as the async function.** `leaf`'s
  `{async_fn#0}` presents as `steps::leaf`, and an async block as
  `steps::main::{async block#0}`.
  - `FunctionInfo` records that the function is the body of a coroutine, and
    of which type.
- **The constructor is a `Wrapper`.** The function that only builds the
  future is never chosen for `break leaf`, and a step passes through it.
- **`break leaf` binds the body.** It binds the `Unresumed` state's entry,
  past the state dispatch, the way Go's function breakpoints go past the
  stack check.
- **The state dispatch is prologue.** Its rows (the header line) are not a
  statement a step or breakpoint stops on.
- **Line breakpoints on an await line bind the arrival path.** Resume points
  are excluded, so the breakpoint fires when execution reaches the await, not
  when the task comes back to it.
  - Drop glue is never bound for a line.
- **Resume points are found statically.** The provider decodes each resume
  function's dispatch on `__state` and maps every state to its target: the
  entry for `Unresumed`, and the resume point of each `SuspendN`.
  - This is a bounded analysis of the entry block, using the existing x86-64
    decoder. A jump table or a compare chain on the state byte are both
    followed.
  - It is checked against a hardware oracle, which steps from the entry with
    each state value written in.
  - A dispatch it cannot decode leaves that function's resume points unknown,
    with a reason. Line breakpoints there say they may also fire on
    resumption. They never silently bind the wrong path.

**Variables of async frames are never stale.**

- **A suspended frame's variables are the fields its state saved:** the
  locals live across that await.
  - The arguments of an `async fn` are moved into the body at its start, so
    their capture fields are hidden once the coroutine has left `Unresumed`.
  - An async block's captures are shown, marked as captures, with the caveat
    that a body may have moved out of one. DWARF records no liveness, and
    nothing is guessed.
- **A running frame's variables come from the resume function's DWARF,**
  never from the future's memory. Optimized code keeps the live copies in
  registers, and memory is authoritative only at a suspend point.
- **A stack-slot local of a resume function is visible only when it was
  assigned in this poll.**
  - While a coroutine runs, its `__state` still holds the state it resumed
    from.
  - A local declared before that state's await line is dead after the
    resumption unless the state saved it, and then the saved copy is shown.
  - This generalizes Go's "visible after its declaration line" rule. The
    never-wrong invariant checks it at every stop.
- **Compiler temporaries are hidden but reachable by name:** `_task_context`,
  `__awaitee`, `__N`, drop flags, and unnamed parameters.
- **`self` (the future) is reachable** as `$future`, a name the provider
  gives the resume function's coroutine parameter.
- **`Poll<T>` return values read**, so `finish` shows `Ready(v)` or
  `Pending`.
- **A coroutine value prints as its state:** `suspended at steps.rs:18 {mine:
  3, …}`, `unresumed`, or `returned`. It never prints as `3 {…}`. The raw
  form is under `[raw]`.

**A task's backtrace is its logical stack.**

- **A running task** (`TaskContext::OnThread`) is its thread's frames.
  - The physical and inline frames of nested resume functions already form
    its async stack.
  - Frames from the task's root `poll` down to the worker's run loop are the
    runtime's. They are marked and folded (see Clients).
  - *As built:* no segment marks them. A tokio task's polls and its
    scheduler share one OS stack, so neither "the task's stack" nor "the
    runtime's stack" would be true. The innermost `Dispatch` frame
    (`Harness::poll`) is where the task's frames end instead:
    `Backtrace::user_frame` never looks past it, and the CLI folds
    everything below it.
- **A suspended task** (`TaskContext::Suspended`) has async frames, innermost
  first, built from its future by the neutral walker:
  - a coroutine frame for each `async fn` or block, at its `SuspendN` line,
    with that state's variables;
  - through `Pin<Box<dyn Future>>`, by the vtable, as `dyn` values are read
    today;
  - through `Box<F>`, `Pin<&mut F>`, and tokio's own boxing of large
    futures;
  - a leaf frame for the first future that is not a coroutine (`Sleep`,
    `Recv`, `Notified`, `JoinHandle`). Its value is presented by its view,
    whose summary says what the task waits for: "sleeping until +1.2 s",
    "the output of task 12", "a permit of a `Mutex`".
- **The leaf's description.**
  - A `JoinHandle`'s target task id is a link the CLI and DAP can select.
  - Deadlines are measured against the runtime's own clock, read from its
    time driver, never the debugger's. A core dump has no "now".
- **Adapters that await several futures,** such as `select!`, `join!`,
  `timeout`, and `JoinSet::join_next`, are a later phase. Until then they
  are a leaf whose value shows their futures. A `select!`'s await line is
  the user's `select!` line, taken from the resume function's own rows for
  the awaitee, as hansei pairs them, not the macro's file.
- **A future driven by `block_on`** is spliced into its thread's backtrace,
  above the `block_on` frame, as its own segment.
  - The tokio model names the driver functions and their future parameter.
    The backend reads that variable in the frame.
  - In an optimized build that leaves it undescribed, the segment is replaced
    by one line saying why.
  - *As built:* the drivers are `CachedParkThread::block_on` (`f`), which
    every multi-thread `block_on` and `Handle::block_on` parks in, and the
    current-thread scheduler's `CurrentThread::block_on` and the closure
    in `CoreGuard::block_on` (each `future`). Only a pinned variable is
    read: an optimized current-thread build describes the moved-from
    argument, whose bytes still read as a future that never began. A
    future shows once, before the innermost frame that reads it; no frame
    below a running coroutine is read, since that future is being polled.
    Where no frame shows it, `Backtrace::unfollowed` says why at the
    driver. At o3, multi-thread drivers lose `f`; current-thread ones keep
    it.
- **A torn chain ends with a typed termination,** never a guess. That covers
  an unreadable future, a discriminant out of range, a cycle, or the depth
  limit.

**Steps follow the task, across awaits.** A step in an async frame belongs
to its task (`StepOwner { task }`, from the thread's activity, as for Go). Its
activation is the coroutine *object* (`Activation::Object`), not a stack
address. The object's address is stable: futures are pinned.

- **`next` over an await whose future is ready at once** is an ordinary line
  step.
- **`next` over an await that returns `Pending`:**
  - The step waits for the same task to resume the same object at that
    await's resume point.
  - Then it continues as the line step it was, and ends at the next line the
    object reaches. That is the line after the await, or another await
    reached in a loop.
  - Mechanism:
    1. When the object's poll returns `Pending`, the step lets the thread run
       until the task's poll returns to the runtime, which is the task at
       rest.
    2. It reads the task's committed chain, which confirms the object and its
       `SuspendN`.
    3. It plants a plan breakpoint at that state's resume point. The
       breakpoint is conditioned on the thread's current task (one
       thread-local read) and, when the same function appears twice in the
       chain, on the object.
  - Hits by other tasks are repaired invisibly, as other goroutines' are.
- **`finish` from an async frame** runs until the object returns
  `Poll::Ready`. A `Pending` return waits for re-entry, as above. It stops
  in the awaiter right after the await, showing the value, or in the runtime's
  harness for a root future, with "task N finished".
- **`step` into `f().await`** passes through the constructor, the future glue
  (`into_future`, `Pin::new_unchecked`, `<Pin<P> as Future>::poll`, `<Box<F>
  as Future>::poll`), and the dispatch, and stops at `f`'s first statement.
- **A task can finish while a step waits:** it completes, panics, or is
  aborted, or the runtime shuts down. The step watches the task's own `poll`
  through its vtable, conditioned on its header, and the runtime's shutdown
  hook.
  - When the task's poll returns with `COMPLETE` set, the step ends: "task 7
    finished", "was cancelled", or "panicked", whichever applies.
  - It never hangs silently.
- **A parent can drop the awaited future,** as `select!`, `timeout`, or
  `abort` do. When the task resumes, each ancestor's re-entry is treated as a
  `next` from its await in which the step's object is a callee.
  - If the ancestor goes on to a new line without polling the object, the
    step ends there and says the awaited future was dropped.
- **Another task's breakpoint, watchpoint, or signal ends the step where it
  happened,** as for Go. uscope's step is never left pending.
- **A step that cannot name its object** (no `self` location, no committed
  chain) degrades to task identity, which is exact except when the same
  function appears twice in the chain. It says when that happened.
- **`step task`** (`StepKind::IntoNewTask`) on a line that spawns steps into
  the new task's first statement, through the task starter `RuntimeModel`
  already has. This is a later phase.

**`step` skips tokio by default; a setting enters it** *(settled)*.

- All of the tokio crate (and `mio`, which only tokio calls) has the code
  role `RuntimeInternal`, as the Go runtime does. A step passes through it to
  the program's code it calls (a task's poll, a callback), and otherwise
  continues as a step out.
  - So `step` into `rx.recv().await` does not enter tokio, as `step` into a
    map assignment does not enter Go's runtime.
- The future glue in `core` and std's panic-catching frames (`catch_unwind`,
  `__rust_try`, `AssertUnwindSafe`) are `Wrapper` and `RuntimeInternal`
  respectively.
- **A neutral setting turns this off for every runtime**:
  - `[step] runtime = "enter"` in the CLI's settings;
  - `set step-runtime on|off` at the prompt;
  - `"stepIntoRuntime": true` in a DAP launch configuration.
  - With it on, roles still mark frames, but steps stop in runtime code. The
    default, `skip`, is today's behavior for Go, so Go's tests stand.
- A step that begins in runtime code may still stop there, as for Go.

**Every Rust panic stops by default** *(settled)*.

- std's runtime model hooks `__rustc::rust_panic`, found by its demangled
  name, since its crate hash varies. std calls it after the panic hook and
  before unwinding begins, so the stop comes before any `catch_unwind`
  decides anything.
- **The stop is a `LanguageException`.** It carries:
  - the message, which the hook has already formatted into the payload;
  - the panic's location, which is the selected frame's line, since
    `#[track_caller]` makes the two agree;
  - when the thread runs a task, the task, with "tokio will catch this panic;
    the task ends with a `JoinError`".
- **The frame selected** is the first below std's and core's panic machinery
  (`core::panicking`, `std::panicking`, `begin_panic`, and `#[track_caller]`
  forwarding in `core`), which get the role `Panic`.
- **The payload is a convention, not DWARF.**
  - std ships with limited debug information: functions, but not its private
    types, so `FormatStringPayload` is in no DWARF.
  - The payload's type is identified by symbolizing its vtable's methods, for
    example `<FormatStringPayload as PanicPayload>::take_box`.
  - Its layout is a convention written down for the pinned rustc and checked
    by a test.
  - A payload of an unknown type stops with "panicked at file:line", and the
    message unavailable with that reason.
- **`panic = "abort"`** stops at the same hook, before SIGABRT.
  `resume_unwind` re-raises without a hook and is reported as a re-raise.
- **Exception stops are declared per runtime.** Each runtime model lists its
  exception kinds with their defaults:
  - Rust: "panics", on;
  - Go: "every panic", off; "unrecovered panics" and "fatal errors", on.

  `ExceptionStops` becomes keyed by runtime and kind. DAP's
  `exceptionBreakpointFilters` lists every known runtime's kinds, since the
  set of runtimes is closed and DAP declares filters before a program is
  known.

**A stop reports every breakpoint hit it contains.** When threads hit
breakpoints together:

- the stop names one, as today;
- the CLI prints the others ("also: task 9 hit breakpoint 2 at workers.rs:13,
  on thread 4122");
- DAP marks each such thread or task stopped by its breakpoint.

Nothing is queued for later stops. This is a client change: the backend
already records each hit in its thread's state.

**What stays out.**

- **uscope never calls the program's functions**, as for Go. tokio's
  `Handle::dump`, `Debug` impls, and `JoinHandle::is_finished` are never run.
  Views stand in for them.
- **Watching a local of an async frame** is refused, with a reason, until a
  later phase defines its scope (the object, while its state keeps the
  local).
- **Freezing or resuming one task alone** is impossible, as for goroutines.

## Architecture

The Go work built the seams this plan uses: tasks, `ExecutionContext`,
`StackSegment`, code roles, `Activation`, `StepOwner`, runtime hooks,
language exceptions, and the boundary tests. tokio is the first runtime
whose tasks have no stacks. Its needs reshape three of those seams, and
nothing else changes shape. Principle 4 of `plans/go.md` said a task is a
scheduling identity, not a stack. This is where that is cashed.

### Layers

| Concern | Lives in | tokio's part |
|---|---|---|
| Coroutine types, resume functions, resume points, code roles | `src/debug_info` | `rust` normalizations: coroutines, async names, roles for tokio, `core::future` glue, panic machinery |
| Static per-module facts | `ModuleImage` | `FunctionInfo.coroutine`; resume points per resume function |
| Dynamic runtime state at one stop | `src/runtime_model` (pure) | `rust/` (panics), `tokio/` (runtimes, tasks, threads, drivers, hooks) |
| Walking a future's chain | `src/runtime_model/futures.rs` (pure, neutral) | none: the walker knows coroutines, not tokio |
| Neutral types | `src/model`, `src/protocol` | `TaskContext::Suspended`, `FrameKind::Async`, `Activation::Object` |
| Process control, unwinding, stepping | `src/backend/linux` | none |
| Library types | `views/tokio.views` | `JoinHandle`, `Mutex`, `RwLock`, `Semaphore`, channels, `Notify`, `Sleep`, `Instant`, `Interval`, `JoinSet`, `task::Id` |
| Clients | `src/cli`, `src/dap`, `src/web` | none beyond names of things |

### Static facts: coroutines

The provider normalizes rustc's coroutine DWARF once, at load, into a neutral
record beside the type:

```rust
/// A state machine a compiler generated for a function that can suspend.
pub struct CoroutineInfo {
    /// The member holding the state number, and its encoding.
    pub state: StateMember,
    /// The states, by number.
    pub states: Vec<CoroutineState>,
    /// Members present in every state: an async block's captures.
    pub captures: Vec<RecordMember>,
    /// The function that runs the coroutine.
    pub resume: Option<FunctionId>,
}

pub enum CoroutineStateKind {
    Unresumed,
    Returned,
    Panicked,
    /// Waiting at an await, which the source shows at `location`.
    Suspended { index: u32, location: SourceLocation, awaitee: Option<MemberRef> },
}
```

- The record is filled from the variant part of each `{async_fn_env#N}`,
  `{async_block_env#N}`, and `{async_closure_env#N}`, and from each variant
  member's `decl_file` and `decl_line`.
  - A state whose file is not the resume function's own, as in a macro's
    awaits, takes its line from the resume function's `__awaitee` locals,
    paired by type. An ambiguous pairing keeps the variant's coordinates and
    marks them as the macro's.
- `TypeKind` gains no variant. A coroutine stays a record whose
  `CoroutineInfo` hangs off its type id.
  - Views, the evaluator, and generic value presentation see a record.
  - The presentation of coroutine values and the async walker read the
    `CoroutineInfo`.
- `FunctionInfo` gains `coroutine: Option<TypeId>` on resume functions, and
  per-function resume points: a state number to a code address, or unknown
  with a reason.
- **Code roles for Rust**, in `src/debug_info/roles.rs`, by path, like
  `go_role`:
  - `Wrapper`:
    - `async fn` constructors (functions returning their own coroutine type);
    - `core::future::into_future`;
    - `Pin`'s forwarding;
    - the `Future` impls of `Box`, `Pin`, and `&mut F`;
    - `AssertUnwindSafe::call_once`.
  - `Panic`: `core::panicking::*`, `std::panicking::*`, and
    `std::rt::begin_panic*`.
  - `RuntimeInternal`: every function in the `tokio` and `mio` crates, std's
    `catch_unwind` and `__rust_try`, and `std::sys` thread-parking.
  - `Dispatch`: tokio's `Core::poll` closure, the frame that hands the thread
    to a task, so a running task's segment ends there.
  - The provider keys on crate paths from DWARF names, never on mangled
    names.

### Dynamic facts: the runtime models

`RuntimeModel` keeps its shape. Its stackful methods (`cross`,
`thread_stacks`, `stack_mover`, `moving_task`, `task_stack`, `call_out`)
answer "none" for tokio, as the codebase audit confirmed they can. These
change:

- **`TaskContext` gains `Suspended { future: VirtualAddress, ty: TypeRef }`.**
  - `task_context` answers `OnThread` for a running task, `Suspended` for
    one at rest, and `Ok(None)` only for a task that does not exist.
  - A task whose future is gone (`Stage::Finished` or `Consumed`) answers
    with a reason, not `None`.
- **The task cursor becomes the model's own.**
  - A linked list resumes from a node, not an index.
  - `TaskCursor.position` becomes an opaque value the model defines, valid
    only at the stop that made it. For tokio it holds the runtime instance,
    the shard, and the next `Header`.
- **`RuntimeModel::drivers()`** names functions that drive a future that is
  not a task, and the parameter holding it: tokio's `block_on`s.
- **`RuntimeModel::exceptions()`** declares the runtime's exception kinds and
  defaults, replacing the fixed `Raised`, `Unhandled`, and `Fatal` defaults.
- **Two hooks for steps.**
  - `task_poll(task) -> (ImageAddress, condition)` gives the function the
    runtime calls to poll a task, and how to recognize this task's call. For
    tokio: `vtable.poll`, with the header in rdi at entry.
  - `task_finished(stop, task) -> Option<TaskEnd>` reads whether the task
    completed, was cancelled, or panicked.

  Go answers neither.
- **`detect` returns a list.** `runtime_model_stays_pure` forbids `tokio::`
  and `use tokio`, rather than the substring. `languages_stay_at_their_seams`
  gains a row for tokio's names (`OwnedTasks`, `CONTEXT`, `current_task_id`)
  and one for std's (`rust_panic`).

How the tokio model answers, at one stop:

- **Runtimes.**
  - Read `CONTEXT` on every thread. The TLS offset comes from the
    `…CONTEXT…__RUST_STD_INTERNAL_VAL` symbol through the existing
    `ThreadLocal` support, and the `eager::Storage` wrapper's `state` must be
    `Alive`.
  - The handle's enum gives the flavor. Runtime instances are grouped by
    handle address.
  - A runtime no thread has entered cannot be found. That is a stated limit,
    not a gap, since nothing proves it exists.
- **Tasks.** Walk each runtime's `OwnedTasks` shards through `Trailer.owned`,
  at `vtable.trailer_offset`. Each node must hold before it is listed:
  - `owner_id` equals the list's id;
  - the vtable is one of the image's task vtables (its `poll` resolves to a
    `raw::poll::<T, S>`);
  - `prev`/`next` agree;
  - the count stays within `OwnedTasks.count`.

  A shard whose mutex is held at the stop may be mid-change. Its tasks are
  listed with that gap. Queued blocking tasks come from the pool's queue.
- **States.**
  - `RUNNING` is `Running`, on the thread whose `current_task_id` names the
    task. With no such thread, the state is `Unknown`, with the reason.
  - `NOTIFIED` and not running is `Runnable`.
  - Neither is `Blocked`, with the leaf's description as `detail`.
  - `COMPLETE` is `Exited`, with "cancelled" or "panicked" as `detail`.
  - Program tasks are not `internal`; worker launch tasks are.
- **Thread activity.**
  - A worker (scheduler context set) whose `current_task_id` is on its
    runtime's list runs that task. Otherwise it is `Idle`.
  - A blocking-pool thread runs its current blocking task, or is `Idle`.
  - A thread entered by `block_on` is the program's own and has no task.
  - Any other thread is not the runtime's.
- **The future.** Go from `vtable.poll` to `raw::poll::<T, S>`, then through
  its `harness: Harness<T, S>` variable's type to `Cell<T, S>`, then to
  `core.stage`. `Running` holds the future, and a `Pin<Box<F>>` is
  dereferenced.
- **The spawn location**, when the build has `tokio_unstable`: when
  `Core.spawned_at` binds, it is the task's `creation`.
  - *As built:* read through the vtable's `spawn_location_offset`, as
    tokio does. A `Location` names a path, not an address, so
    `TaskLocation.address` is optional and its `recorded` place is matched
    to a source file of the module's image. A running blocking closure is
    known only by its number, with no header, so its spawn location is not
    known. `task` alone prints `created at …`, and a goroutine's `created
    by …`, except the main goroutine's, which Go's traceback leaves out.
- **The entry** is the root future's resume function, at its `Unresumed`
  location, the "defined at" place.

### The future walker

`src/runtime_model/futures.rs` is pure and neutral. Given a future's address
and type, a `RuntimeStop`, and a way to look up a type's `CoroutineInfo` and
vtables, it returns `Partial<Vec<AsyncFrame>>`:

```rust
pub struct AsyncFrame {
    pub object: VirtualAddress,
    pub ty: TypeRef,
    pub kind: AsyncFrameKind, // Coroutine { state, location } | Leaf
}
```

- It follows `__awaitee`, `Box`, `Pin`, and `dyn Future` (through the
  existing vtable-to-type index).
- Its depth is bounded, and it detects cycles by object address.
- Every failure is a typed termination.
- It is fuzzed on arbitrary memory: it must never panic, must always end, and
  must report a typed reason.

### Neutral types

Each change below is a refactor of the kind `plans/go.md` principle 6 asks
for: its own commit, behavior unchanged, the whole suite passing, before
tokio depends on it.

- **`StackRoot` gains `Suspended { future, ty }`.**
  - `walk_stack` produces async frames for it through the walker, with no
    registers.
  - `FrameRegisters` gains no variant. An async frame has none, and its
    variables are evaluated relative to its object.
- **`FrameKind::Async`,** with the frame's object.
  - `StackFrame.instruction` is `None` for it.
  - `source` is its await location.
  - `segment` is `Task`.
  - `ResolvedFrame` gains an object base. The variable evaluator reads the
    state's members from the object; it never runs DWARF location
    expressions there.
- **`StackSegment` gains `Future`,** for a driver's spliced future.
  - A running task's frames are `Task` down to the `Dispatch` frame, and
    `System` below it.
- **`Activation` gains `Depth::Object(VirtualAddress)`,** with owner
  `Task(TaskId)`.
  - `same` compares addresses.
  - `has_returned` reads the object's state: `Returned`, `Panicked`, or a
    task that no longer holds the object.
  - It is never ordered against a CFA.
  - The roughly 20 predicate sites compare owners first, so they stay correct
    by construction. Steps learn the new waits.
- **`TaskState` gains `Exited`,** which the Go plan reserved. `Suspended` is
  not added: a suspended future is `Blocked` on its leaf.
- **`$task`** keeps returning the task's number. tokio ids are unique in a
  process, and Go has one runtime per image, so the dropped runtime id is
  harmless. A process with two Go runtimes keeps its existing ambiguity
  error.

### Keeping it clean

- tokio is named in `src/runtime_model/tokio`, `src/debug_info` (roles), and
  `views/tokio.views`. Rust's panic runtime is named in
  `src/runtime_model/rust`. Nothing else names either, and the seam test
  enforces it.
- The future walker names no runtime and no language.
- Backend tests use a fake runtime model whose tasks are suspended futures in
  `FakeTrace` memory. That tests async backtraces, task-owned steps across an
  await, and finished-task endings without a tokio binary.
- The simulator is not given a runtime, as for Go. Its existing sweeps must
  stay green through the phase-0 refactors.

### Clients

- **CLI.**
  - `tasks` lists tokio tasks with the existing columns:
    - id;
    - where the program's code is, the innermost user await;
    - state and what it waits on;
    - thread.
  - `tasks -g` groups by await location with counts: hansei's census and
    Visual Studio's grouping, already built for Go.
  - `task N` selects a task. `bt`, `frame`, `up`, `down`, `print`, and
    `info locals` then work on its async frames.
  - **Backtraces fold the runtime.**
    - A run of `RuntimeInternal` and `Dispatch` frames prints as one line,
      such as `… 18 frames of tokio's scheduler (worker 2) …`. *As built:*
      `… #4–#21: 18 frames of the runtime; `bt -r` shows them`, for every
      runtime, Go's too, as the maintainer chose.
    - `bt -r` prints them all. Nothing is dropped: the frames still exist,
      DAP still lists them, and selecting by number still counts them.
    - This refines Go's settled "every frame is shown" for runtimes whose
      machinery is tens of frames deep, and would apply to Go's runtime
      frames too. *(Proposed: it changes a settled Go decision, so it needs
      the maintainer's agreement before phase 4.)*
  - **Rust frame names elide long generic arguments** as `<…>` in
    backtraces, keeping the path and the method.
  - Async frames print as `#3 async steps::middle (steps.rs:18) awaiting
    leaf`. The leaf prints as `#0 awaiting tokio::time::Sleep — sleeping
    until +1.2 s`.
  - Panic stops print the message, the location, and the task.
- **DAP.**
  - With `"threads": "tasks"`, the default, tokio tasks are DAP threads, as
    goroutines are. Names look like `[7] steps::middle — sleeping (thread
    4122)`.
  - System threads that run no task are listed too, except a runtime's idle
    threads: workers and pool threads with nothing to run. Threads the
    program spawned itself stay visible.
  - Async frames are stack frames with their sources. Segment changes are
    `label` frames ("awaiting", "tokio scheduler"), and runtime frames are
    `subtle`.
  - `exceptionInfo` explains panics. The exception filters come from
    `RuntimeModel::exceptions()`.
- **Web UI.** It has no tasks today, for any runtime. A tasks panel serves
  both Go and tokio:
  - a list with grouping;
  - selecting a task points the stack, variables, and source at it, through
    `ExecutionContext` in the web protocol;
  - the address bar holds the selected task.

  `protocol.gen.ts` is regenerated.

## Work, in order

Each phase ends with its fixtures, docs, and README updated, and with
`just all`. Run-control phases also run `just stress` and `just sim 600`.
Each failure from the experiments becomes a test written first and seen to
fail (see Testing).

0. **Neutral seams, with no change in behavior.** Each is its own commit,
   and the whole suite passes on it:
   - several runtimes per module, with `RuntimeId` decoupled from `ModuleId`;
   - the model-defined task cursor;
   - `TaskContext::Suspended`, `StackRoot::Suspended`, `FrameKind::Async`,
     `StackSegment::Future`, and `TaskState::Exited`, all unused;
   - `Activation`'s `Object` depth;
   - exception kinds declared per runtime, with Go's existing defaults;
   - the step setting, defaulting to today's behavior;
   - the boundary tests' new rows.
1. **Fixtures and Rust foundations.**
   - The Nix-vendored fixture workspace, the `truth` crate, and the build
     matrix.
   - Coroutine normalization and the coroutine contract test.
   - Async names. The constructor as `Wrapper`, and `break f` binding the
     body.
   - Resume points and their hardware oracle. Await-line breakpoints bind
     arrival, and no line binds drop glue.
   - Locals hygiene: saved over stale, hidden temporaries, `$future`, and
     `Poll<T>` returns.
   - Coroutine values printed as states.
   - The Rust value bugs found on the way:
     - `Arc`/`Rc` deref and `*arc`;
     - `UnsafeCell` transparency;
     - `print/x` on atomics;
     - "variant part has neither a discriminator nor a tag type".

   Most of this helps every async Rust program, with or without tokio.
2. **Rust panics.**
   - std's runtime model and the `rust_panic` hook.
   - The payload convention and its test.
   - `Panic` roles and the selected frame.
   - `panic = "abort"`.
   - Per-runtime exception filters in the CLI and DAP.
3. **tokio tasks.**
   - The tokio model: contract, version gate, `CONTEXT`, runtimes, owned and
     blocking tasks, thread activity, states, futures, and spawn locations.
   - tokio's code roles and `Dispatch`.
   - Tasks in the CLI, DAP, and cores.
   - Co-hit reporting.
   - The `just soak` recipe, first run at this phase's end.
4. **Async backtraces.**
   - The future walker and suspended tasks' async frames, with their
     variables.
   - Running tasks' segments, and backtrace folding.
   - `block_on` splicing.
   - Task panics name their task.
   - *As built:* leaf descriptions moved to phase 6, since a description
     is a view's summary and the views are written there.
5. **Run control across awaits.**
   - Task-owned steps for tokio.
   - `next` and `finish` across `Pending`.
   - The finished, cancelled, and dropped endings.
   - `step` into `.await`.
   - The step setting.
   - `$task` conditions.
6. **Views.** `views/tokio.views`, each type verified by `VIEW:` markers
   against the pin.
   - Leaf descriptions through views, with `JoinHandle` links.
7. **The rest.**
   - `LocalSet` tasks, found while their set runs or from the frame that
     runs it.
   - The current-thread runtime throughout.
   - Several runtimes in one process.
   - Attach.
   - Scale.
   - The web UI's tasks panel.
   - `step task`.
   - `docs/tokio.md`, and the README's Rust row.
   - Remove the TODO line.

Later, only if a need appears:

- `select!`, `join!`, and `JoinSet` as branching async frames;
- a wait-for graph across tasks (`JoinHandle` and semaphore edges);
- watchpoints on async locals;
- tokio-console-style poll statistics, which need instrumentation;
- other executors through the same walker.

## Facts the plan depends on

Verified on 2026-10-07 with tokio 1.52.3 (spot-checked against 1.51.0 and
1.53.1), rustc 1.99.0-nightly (2026-07-10), gdb 17.1, and tokio's sources.

- **DWARF and symbols.**
  - The pinned rustc writes DWARF 4 and mangles symbols in v0 (`_R…`).
  - Every tokio type this plan reads has member offsets in debug, O2, and O3
    builds. Debug and release layouts were byte-identical.
  - `Cell<T, S>`, `Core<T, S>`, `CoreStage<T>`, and `Stage<T>` appear once
    per instantiation.
  - `debuginfo = "line-tables-only"` keeps no types or variables. Tasks could
    still be listed with hard-coded offsets, which this plan refuses to
    hard-code.
  - std ships limited debug information: its functions, but not its private
    types.
- **Task layout (1.52.3, x86-64).**
  - `Header` is 32 bytes: `state` at 0, `queue_next` at 8, `vtable` at 16,
    and `owner_id` at 24.
  - `Cell` is 128-aligned, with `header` at 0 and `core` at 32.
  - `Core` has `scheduler` at 0, `task_id` at 8, and `stage` at 16.
  - `Stage` is a u32 tag (0 `Running`, 1 `Finished`, 2 `Consumed`) with its
    payload at +8.
  - The vtable has seven function pointers, then `trailer_offset`,
    `scheduler_offset`, and `id_offset`. The stage's offset is not in it.
- **Finding the future's type.** `vtable.poll` points at `raw::poll::<T, S>`,
  whose DIE has `T` and `S` template parameters in every build. Release
  builds merge identical functions, so `S` is never taken from a frame's
  name.
- **`CONTEXT`.**
  - It is a native, eagerly initialized thread-local in `.tdata`, at
    `fs_base − roundup(PT_TLS.memsz, align) + offset`.
  - Its type is `eager::Storage<Context>`: the value at 0, and `state` at 72
    (0 alive, 1 never touched).
  - In `Context`, `current.handle` is an `Option<scheduler::Handle>` (0
    CurrentThread, 1 MultiThread, 2 None) holding an `Arc`.
  - `current_task_id` is at 48. On an idle worker it holds the worker's own
    launch task.
- **From the handle to the list.**
  - Multi-thread: `Arc` data → `shared` → `owned`.
  - Current-thread: `Arc` data at +128, because the handle is 128-aligned,
    then `shared` → `owned`.
  - `OwnedTasks` holds the shards (pointer and length), `count`,
    `shard_mask`, and the list `id`.
  - Each shard is a `Mutex<LinkedList>` of 24 bytes: the futex word at 0,
    `head` at 8.
- **The walk works.** It found exactly the 10 tasks a fixture created. Run
  queues are subsets of the owned list.
  - A completed task is reachable only through its `JoinHandle`.
  - A `LocalSet`'s tasks hang from its `Rc<Context>` → `Arc<Shared>` →
    `local_state.owned`.
  - `task::local::CURRENT` is null while the set is parked.
- **Coroutines.**
  - Each has a variant part keyed on `__state: u8`, at an offset that
    varies.
  - Variant members carry `decl_file` and `decl_line`:
    - the header line for `Unresumed`;
    - the closing brace for `Returned` and `Panicked`;
    - the await's statement line for `SuspendN`.
  - There are no columns. Debug and optimized builds agree.
  - `select!`'s await points into `tokio/src/macros/select.rs`.
  - A running task's discriminants read as the state it resumed from.
  - `Pin<Box<dyn Future>>` awaitees resolve through a DWARF vtable variable
    whose `{vtable_type}` has `DW_AT_containing_type`.
- **Dispatch.**
  - In debug builds a resume function begins with `movzbl state(%rdi)` and a
    jump through a relative table in `.rodata`. Those rows carry the
    function's header line.
  - In release builds the bodies are inlined into `raw::poll::<T, S>`, and
    no `{closure#0}` symbol remains.
- **Threads.**
  - An idle worker's stack runs `futex` (or `epoll_wait`, `mio`, and the IO
    driver), `park_condvar`, `Parker::park`, and `worker::Context::park`,
    down through `run`, `launch`, the blocking pool's harness, and
    `start_thread`.
  - Workers and pool threads are both named `tokio-rt-worker`.
- **Identity.**
  - `task::Id` comes from a process-wide counter.
  - Without `tokio_unstable`, no spawn location and no name is stored
    anywhere.
- **Version 1.53.1** adds a zero-sized `Header.scheduled_at`, and moves
  `OwnedTasks` within the multi-thread `Shared` from 112 to 128. Reading by
  name absorbs both.
- **Panics.**
  - `__rustc::rust_panic(&mut dyn PanicPayload)` is called after the hook.
  - The hook calls `payload.get()`, which formats a `FormatStringPayload`'s
    message into its `string` field.
  - `resume_unwind` calls `rust_panic` without running the hook.
- **Metrics.** `RuntimeMetrics::worker_park_unpark_count` is stable on 64-bit
  targets: "an odd count means that the worker is currently parked."
  `num_alive_tasks` is stable too.

## Testing

The suite must prove that real tokio programs, built by the pinned
toolchain against the pinned tokio, debug correctly from start to finish.
That covers happy paths, refusals, broken programs, and the races that come
from many workers polling many tasks.

Each test must be worth what it costs, so the suite is built mostly from
end-to-end sessions against real example programs. Narrower techniques are
used only where the logic is intricate and its input is adversarial.

A test earns its place when all of these hold:

1. **It drives real compiler output through the public request path:**
   `tests/support::Scenario`, the DAP harness, or the web server. Pure
   layers are the exception, tested below their callers only where noted.
2. **It checks against an oracle uscope did not compute.** Comparing uscope
   with itself proves nothing.
3. **It fails for a bug a user would see.**
4. **It is deterministic,** waiting only for what it can observe (AGENTS.md):
   an event, a debugger state, a `/proc` fact, or a line the fixture prints.

One stop that checks many properties beats many tests that each check one.
Every behavior in the catalog below has a home in some test, but most homes
are shared: one checkpoint of the worker pool verifies dozens of them.

### Methods, and where each is used

| Method | What it is for | Where it is used |
|---|---|---|
| End-to-end scenarios | Every user-observable behavior, on real programs | `tests/tokio`, the bulk of the suite |
| Checkpoint cores | The same assertions without a live process, deterministically | Every quiescent checkpoint, replayed on its core |
| DAP sessions and replayed traffic | Behavior as an editor sees it | `tests/dap/tasks.rs`; one VS Code and one nvim-dap recording |
| Web end-to-end | The tasks panel and shared links | `web/e2e`, Playwright |
| Property tests | Pure algorithms whose correctness is a law over many inputs | The future walker, the task-list walker, paging, state decoding, activations |
| Fuzzing | Code that reads untrusted memory or machine code | The walkers, coroutine normalization, the dispatch decoder, the panic payload |
| Hardware oracle | A static analysis checked against what the CPU actually does | Resume points |
| Independent parsers | Static facts checked against a reader uscope did not write | `readelf` for coroutine DWARF; std's own panic output |
| Stress | Races that appear one run in hundreds | `just stress` over `tests/tokio` |
| Soak | Leaks, drift, and rare tears that need minutes of real load | `just soak`, one recipe, at the end of phases 3, 5, and 7 |
| User acceptance | The whole experience in real editors | `just uat-vscode`, `just uat-nvim`, and a written walkthrough |
| Sabotage | Proof that each oracle catches the lie it exists for | Every new oracle and invariant |
| Simulator sweeps | That the neutral run-control refactors broke nothing | Existing sweeps only; tokio is not simulated |

**Mutation testing** is not used, by standing decision. Sabotage tests
cover the oracles instead.

### Oracles

Truth comes from outside uscope, in this order of preference.

- **The program reports its own truth.** A small `truth` library in the
  fixture workspace gives every fixture the same tab-separated `TRUTH`
  lines.
  - **Tasks.**
    - Each fixture task records its id (`tokio::task::id()`) when it
      starts, and removes it when its body ends.
    - At a checkpoint the live set is printed with
      `RuntimeMetrics::num_alive_tasks()`, which must agree with it.
  - **Await sites.**
    - Before each await a test cares about, a task holds a guard naming
      that await's tag for as long as the await lasts. The source line
      carries a matching `// AWAIT: tag` marker.
    - The nesting of guards across a task's async functions is its logical
      stack, which the program prints innermost first.
    - Tests find lines by marker, never by number.
  - **Values.**
    - Each task records the values of the locals its checkpoint names:
      integers exactly, floats as bits, strings and collections by
      contents.
    - These are the values uscope must show for the saved locals of each
      async frame.
  - **Threads.** A task records `gettid()` each time it resumes, which proves
    migrations happened and names the thread a running task should be on.
  - **Panics.** The fixture's own panic hook records the message and the
    location (`PanicHookInfo`), then chains to the default hook.
  - **Hits.** Fixtures count how often each instrumented line runs, per task.
    Breakpoint hit counts are checked against these counts.
  - **The program's state at a stop.** Every fixture task keeps `me`, its
    own id, in a local saved across its awaits. A step that ends with `me`
    changed ended in the wrong task, whatever uscope believes.
- **The native run.**
  - Every fixture also runs without a debugger at build time. Its exit
    status, standard output, and standard error are recorded beside it, as
    Go's are.
  - A debugged run must end the same way.
  - A fixture that misbehaves on its own fails the build, not a debugger
    test.
- **The same stop as a core.**
  - gdb's `gcore` saves each fixture's main checkpoint at build time.
  - Every assertion made live at that checkpoint runs again on the core:
    tasks, thread activity, async backtraces, values, and views.
- **`readelf` for coroutine DWARF.**
  - At build time, `readelf --debug-dump=info` output is reduced to every
    coroutine type's states, their `decl_line`s, and their awaitee types,
    and recorded beside the fixture, as the `gosym` oracle is for Go.
  - uscope's normalization must agree for every coroutine in every fixture.
    The macro-pairing rule is the one documented exception, and it must
    agree with the `// AWAIT:` markers.
- **The CPU, for resume points.**
  - For each resume function in the fixtures, at a stop where its future
    sits suspended in memory, a test writes each state value into a copy of
    the future and steps instructions from the function's entry.
  - The address where the dispatch leaves for the body is ground truth for
    that state. The provider's static map must equal it for every state of
    every resume function, in both builds.
- **tokio's own metrics.** `num_alive_tasks`, `num_workers`, and
  `worker_park_unpark_count` (odd while parked) are tokio's statements about
  itself. They are used for counts and for quiescence, never in place of the
  program's own records.

gdb, BugStalker, and hansei are not oracles. gdb cannot read async state.
BugStalker's known errors (one task per shard, stops on dispatch rows) would
be imported by agreeing with it. hansei reads cores only, and its license
keeps its code out of this repository.

### Fixtures

Every fixture is a binary in the Nix-vendored workspace
`tests/fixtures/rust/tokio`, using tokio 1.52.3 and `truth` only. One
fixture, `std-async`, needs no crates at all. A program here is a real
program, kept small: each forces the situation it exists for, and prints
the evidence that the situation arose.

| Fixture | What it forces |
|---|---|
| `std-async` | Async functions under a ten-line `block_on` executor written with `std::task::Wake`, and no tokio. Phase 1's naming, breakpoints, steps within a poll, values, and locals hygiene are tested here before tokio exists in the suite. |
| `workers` | A multi-thread runtime with eight tasks in one `async fn`, parked at different awaits: an `mpsc` receive, a long `sleep`, a `Mutex` lock, a `JoinHandle`, `Notify`, a `oneshot`, `yield_now`, and a `Semaphore` acquire. Nested async functions are three deep, with locals saved across each await. |
| `steps` | A loop body whose awaits pend, or are ready, under the test's control: a gate future the test opens by writing to a pipe. Siblings run the same function throughout. Variants run on the current-thread runtime and in a `LocalSet`. |
| `migrate` | A task that resumes on another worker. Its last worker is held in a `block_in_place` section until the task has been woken from a thread outside the runtime, so the wake goes through the inject queue. The program prints both `gettid`s. |
| `cancel` | An awaited future dropped by `select!` taking a ready branch; a `JoinHandle::abort` from another task; and a runtime shut down with tasks pending. Each is triggered by the test's handshake. |
| `panics` | Task panics: a `&'static str`, a formatted message, `unwrap` on `None`, `expect` on an `Err`, and `panic_any` of an `i32`. Also a panic in `spawn_blocking`, in a `LocalSet` task, and in `main`; a panic inside the program's own `catch_unwind`; `resume_unwind`; a panic in `Drop` during unwinding; and a program-installed panic hook. |
| `blocking` | A one-thread blocking pool with one blocker running and two queued. |
| `drivers` | `#[tokio::main]` with the main future parked in `block_on`; `Runtime::block_on` called on a plain thread; and `Handle::block_on`. |
| `shapes` | Every shape of await chain: recursion through `Box::pin`, `Pin<Box<dyn Future>>`, a generic async function at three instantiations, an async closure, an async trait method, an async block, a future over 16 KiB (boxed by tokio), a hand-written `impl Future`, a `pin!`ned future awaited by reference, `tokio::join!`, `select!`, `timeout`, and `JoinSet`. |
| `values` | Every tokio type with a view, in every state its view distinguishes. Also `Arc`/`Rc`, `UnsafeCell`, `Poll`, and coroutine values in each state. |
| `server` | An echo and HTTP-style line server on `127.0.0.1:0`, one task per connection, with a client mode. It is the attach target, the soak target, and the real-world program. |
| `scale` | 100,000 parked tasks, then optionally churn: tasks spawned and completed continuously, each bracketing its spawn and its end so that the truth names the tasks in transition. |
| `deadlock` | Two tasks each holding one `tokio::sync::Mutex` and waiting for the other's. |
| `corrupt` | Written with `unsafe` after a checkpoint, then another checkpoint: a `Header` whose `owner_id` is wrong, a broken list link, a cycle, an out-of-range `__state`, and a vtable pointer into data. |
| `forked` | A tokio program that forks; the child stops itself before doing anything else. |
| `two-runtimes` | A multi-thread runtime and a current-thread runtime in one process, each with parked tasks. |

**Quiescent checkpoints.** A checkpoint's assertions are exact, so
nothing may be moving. The checkpoint runs on a thread outside the runtime,
or in `block_on`, and calls `reached(name)` only after all of these hold:

- every task it expects has registered the await it is parked at;
- every worker is parked, by an odd `worker_park_unpark_count`;
- every blocking-pool thread is idle or in its own known blocker;
- no timer can fire before the test continues, since sleeps are hours long.

Nothing waits by sleeping. Tests about running tasks use breakpoints, not
checkpoints, and assert only what their stop determines.

### Build matrix

As for Go, each axis is tested only where it changes behavior. Axes are
not multiplied into a Cartesian product.

| Axis | Why it matters | Fixtures |
|---|---|---|
| `opt-level=0` against `opt-level=3`, both `debug=2` | Inlined resume functions, the dispatch's shape, optimized-out locals, merged functions | Every one but `corrupt` and `scale` |
| Current-thread against multi-thread | Handle layout, thread activity, `block_on` drivers | `workers`, `steps`, `panics`, `drivers` |
| `--cfg tokio_unstable` | Spawn locations; a vtable with one more offset | `workers` |
| `panic = "abort"` | The panic hook before SIGABRT; a crash core | `panics` |
| `debug = "line-tables-only"` | Every task feature refused with its reason, never wrong | `workers` |
| `strip = true` | Symbols only | `workers` |
| `-C symbol-mangling-version=legacy` | Finding `rust_panic` and tokio's functions by demangled name | `panics`, `workers` |
| `--remap-path-prefix` over the vendored crates | tokio's version unknown: the gap, not a guess | `workers` |

### Invariants at every stop

Every tokio scenario is built `checked`, which runs `check_tokio_stop`
(`tests/tokio/invariants.rs`) after every stop, as `check_go_stop` does for
Go. Most bugs surface here, in whichever test happens to reach them.

- **Never wrong.** Every value shown as available equals the truth the
  program recorded for it. A value uscope cannot show is unavailable, with a
  typed reason. In optimized builds unavailable is allowed; wrong never is.
  This is where stale stack slots after a resume are caught.
- **Tasks agree with the program** at a quiescent checkpoint:
  - the listed ids are exactly the program's live set, and their count is
    `num_alive_tasks`;
  - each task's state is `Blocked`, with its leaf's description;
  - each task's innermost user await is the line of its registered tag;
  - each task's async frames, innermost first, are the nesting of its
    guards, each at its marker's line, with exactly the recorded locals.
- **Every thread is accounted for.** Each thread runs a task, is a runtime's
  idle thread, is a `block_on` thread, or is not a runtime's thread at all.
  None is unknown without a reason. A thread running a task is the thread
  whose last recorded `gettid` the task wrote.
- **Every backtrace ends properly.**
  - A thread's backtrace ends `Complete`.
  - A suspended task's ends at its root future, or with a typed
    termination.
  - No async frame repeats an object.
  - Segments change only at a `Dispatch` frame, at a driver, or at a stack
    switch.
  - No frame of a fixture's own code is unnamed.
- **Steps keep their identity.** A completed `next`, `step`, or `finish`:
  - ends with `me` unchanged;
  - ends on a statement row that is neither a dispatch row nor a resume
    point's entry;
  - for `next` and `finish`, ends in the same coroutine object or its
    awaiter, as the program's guards show.
- **Hits are counted once.** Each breakpoint's hit count equals the
  program's own count of executions of that line, at every stop.

### Behavior catalog

These are the behaviors the suite must show. Each row names its kind:

- **H**, a happy path;
- **S**, a sad path: a refusal, an error, or a broken program;
- **E**, an edge case;
- **C**, a complicated or concurrent case.

Each row also names its home. Unless a row says otherwise, it holds in both
builds, live and on the checkpoint's core.

#### A. Finding runtimes and tasks

| Behavior | Kind | Home |
|---|---|---|
| A multi-thread runtime's tasks are listed exactly, with their count | H | `workers` |
| A current-thread runtime's tasks are listed through its `block_on` thread | H | `workers` (current-thread) |
| Two runtimes in one process are both found; each task names its runtime; ids are unique | H | `two-runtimes` |
| A task woken but not yet polled is `Runnable` | H | `workers` (current-thread: the main future wakes a task, then checkpoints before yielding) |
| A task spawned but never polled is `Runnable`; its one async frame is "not started", at its function's header | E | same |
| A task running at a breakpoint is `Running`, on the thread whose `me` matches | H | `steps` |
| A completed task whose handle lives is not listed; its handle's view shows the output | E | `values` |
| A queued `spawn_blocking` task is listed `Runnable`, "queued in the blocking pool"; a running one is `Running` on its thread | H | `blocking` |
| Worker launch tasks are hidden, and shown as the runtime's own with `tasks -a` | H | `workers` |
| With `tokio_unstable`, a task's creation is its `spawn` line | H | `workers` (unstable) |
| A future tokio boxed is listed under its own function, not `Pin<Box<…>>` | E | `shapes` |
| Stopped at `main`'s first line, before any runtime exists: no tasks, and no gap | E | `workers` |
| After the runtime is dropped: no tasks, and no stale ones | E | `cancel` |
| A forked child lists none of the parent's workers' tasks as running, and says the runtime's workers are not in this process | E | `forked` |
| A thread that never touched tokio is not a runtime's, never unknown | E | `two-runtimes` (its plain thread) |
| A `Header` with a foreign `owner_id`, a broken link, or a cycle: the task, or the rest of its shard, is reported with its reason, and every other shard is listed | S | `corrupt` |
| A vtable pointing into data: that task is refused with its reason | S | `corrupt` |
| The list's count disagrees with the walk: the page carries the gap | S | property tests, then `corrupt` |
| A shard whose mutex is held at the stop: its tasks carry "was being changed" | E | property tests; seen live in the soak |
| `line-tables-only`: `tasks` says which type is missing; nothing is listed from a guessed offset | S | `workers` (line-tables-only) |
| Stripped: tasks are unavailable with their reason, and frames are named by symbols | S | `workers` (stripped) |
| tokio's version unknown (remapped paths): every result carries the gap | S | `workers` (remapped) |
| An unverified version: every result carries "tokio 1.X is unverified" | S | the model's unit test, renaming the compile unit's path |
| A contract name missing: only the feature needing it is unavailable, naming the field | S | the model's unit test, once per name group |
| A Rust program without tokio: `tasks` says the program has no tasks; panics still stop | H | `std-async` |

#### B. Thread activity

| Behavior | Kind | Home |
|---|---|---|
| A worker polling a task runs that task, on the `Task` segment down to `Dispatch`, with `System` below | H | `steps` |
| An idle worker, parked on a condvar or in the IO driver, is the runtime's idle thread | H | `workers`, `server` |
| A blocking-pool thread runs its blocker's task, or is idle | H | `blocking` |
| A `block_on` thread runs no task and is the program's own | H | `drivers` |
| A thread in `block_in_place` runs the same task, and the worker that took over its core is a worker | E | `migrate` |
| A task seen on one thread at one stop is on another at a later stop, and `me` and `gettid` agree | C | `migrate` |
| A plain `std::thread` with its own code is listed as the program's thread | H | `two-runtimes` |

#### C. Async backtraces and variables

| Behavior | Kind | Home |
|---|---|---|
| A suspended task's async frames match its guards, innermost first, each at its marker's line | H | `workers` |
| Each async frame's locals are exactly the locals saved at that await, with the recorded values | H | `workers` |
| Each leaf names what its task waits for: the channel, the sleep's deadline on the runtime's clock, the `Mutex`, the permit, `Notify`, the `oneshot`, `yield_now` | H | `workers` |
| A `JoinHandle` leaf names its task, and `task N` follows it | H | `workers` |
| `up`, `down`, and `frame N` move across async frames; `info locals` and `print` work in each | H | `workers` |
| `print` evaluates expressions over saved locals; `$future` is the frame's future | H | `workers` |
| `set var` on a saved local writes the future, and the program, once resumed, prints the new value | H | `steps` |
| Registers and `disassemble` in an async frame are refused, saying the frame is suspended | S | `workers` |
| `async fn` arguments are not listed twice, and their capture fields are hidden after the body starts | E | `workers` |
| An async block's captures are listed as captures, marked "may have been moved" | E | `shapes` |
| Compiler temporaries (`_task_context`, `__awaitee`, `__N`, unnamed parameters) are not listed, and are reachable by name | E | `std-async` |
| Recursion through `Box::pin`, 50 deep: 50 frames, each its own object | E | `shapes` |
| Through `Pin<Box<dyn Future>>`: the concrete function is named | E | `shapes` |
| A generic async function at three instantiations: each frame's types resolved | E | `shapes` |
| Async closures, async trait methods, and async blocks are named by their function and a block number | E | `shapes` |
| A hand-written `impl Future` is a leaf, shown with its value | E | `shapes` |
| `join!`, `select!`, `timeout`, and `JoinSet` are leaves whose values show their futures; a `select!` frame's line is the user's `select!` line, not tokio's macro file | E | `shapes` |
| A chain past the depth limit ends with the typed limit | E | property tests; `shapes` (recursion 10,000 deep) |
| An out-of-range `__state` ends the chain with its reason; the frames above it stand | S | `corrupt` |
| A running task's own frames are physical and inline; its future's memory is never read for them | H | `steps` |
| In release builds, inlined async functions appear as frames named for the async function | H | `steps` (O3) |
| A local declared before the await this poll resumed from, and not saved, is not shown | E | `steps` (loop variable, shadowed variable) |
| A local declared after that await is shown | H | `steps` |
| The `block_on` future is spliced into the main thread's backtrace as its own segment, in debug builds | H | `drivers` |
| Where optimization leaves the `block_on` future undescribed, one line says so | S | `drivers` (O3) |
| A runtime's frames fold into one line in the CLI, and `bt -r` shows them; frame numbers count every frame | H | CLI test |

#### D. Breakpoints

| Behavior | Kind | Home |
|---|---|---|
| `break leaf` binds the body once, past the dispatch, in both builds, never the constructor | H | `std-async`, `steps` |
| `break` on a generic async function binds every instantiation | H | `shapes` |
| `break` on an `async fn`'s header line binds the body's entry, not the dispatch | E | `steps` |
| A breakpoint on an await line that never pends fires on every arrival; its hit count equals the client's requests | H | `server` |
| A breakpoint on an await line that pends fires once per arrival, never on resumption | H | `steps` |
| No line binds drop glue | E | `steps` (location count checked) |
| A function whose dispatch cannot be decoded: its await-line breakpoints say they may also fire on resumption | S | the dispatch decoder's unit tests on generated code |
| `break … if $task == N` stops only in task N, with hit counts matching the program's | H | `workers` |
| Hit counts across eight tasks running one line match the program's counts | C | `steps` |
| Breakpoints in `spawn_blocking` closures and in synchronous functions called from async code | H | `blocking`, `steps` |
| Four workers hitting one breakpoint together: one stop that reports all four, and no hit lost or double-counted over 1,000 hits | C | `steps`; stress |
| Deleting a breakpoint while a step waits leaves the step intact | E | `steps` |

#### E. Stepping

| Behavior | Kind | Home |
|---|---|---|
| `next` over a ready await is a line step | H | `std-async`, `steps` |
| `next` over a pending await ends on the next line, in the same task, after the test opens the gate | H | `steps`, all three runtimes |
| The same, when the task resumes on another thread | C | `migrate` |
| `next` over an await in a loop stops at each iteration's lines, per the `// WALK:` markers | H | `steps` |
| `next` at an async function's last line ends in its awaiter, after the await | H | `steps` |
| `next` at the root future's last line ends with "task N finished" | E | `steps` |
| `next` over `tokio::spawn(…)` does not enter the new task | H | `steps` |
| `step` into `f().await` stops at `f`'s first line in one step, through an async fn, an async block, a boxed `dyn` future, a generic, and a trait method | H | `std-async`, `shapes` |
| `step` into `rx.recv().await` steps over tokio; with `step-runtime on`, it stops inside tokio; turning the setting back off restores the default | H | `steps` |
| `finish` from a nested async function after zero, one, and many pending polls returns to the awaiter, showing the value | H | `steps` |
| `finish` from the root future ends with "task N finished" and its output | E | `steps` |
| `finish` from a hand-written `poll` shows its `Poll<T>` as `Ready(v)` or `Pending` | H | `std-async` |
| `task N` then `next` on a suspended task waits for it to resume and stops at its next line | H | `steps` |
| While a step waits, siblings hitting the step's plan breakpoints are repaired invisibly and counted nowhere | C | `steps` |
| While a step waits, another task's user breakpoint ends the step there, with the step reported incomplete | C | `steps` |
| While a step waits, the task is aborted, finishes, or panics: the step ends saying which | S | `cancel`, `panics` |
| While a step waits, `select!` drops the awaited future: the step ends at the parent's new line, saying the future was dropped | C | `cancel` |
| While a step waits, the runtime shuts down: the step ends saying the task was cancelled | S | `cancel` |
| `pause` while a step waits: the step is reported incomplete, and a new step works | E | `steps` |
| `kill`, `detach`, or the program exiting while a step waits: the session ends cleanly; on detach the program's output matches the native run | S | `steps`, lifecycle |
| A step whose object cannot be named in an optimized build continues by task identity and says so; a recursive chain makes it refuse | S | `shapes` (O3) |
| `stepi`, `until`, and `advance` inside a resume function behave as in any function | E | `steps` |
| `step task` on a `spawn` line stops at the new task's first line | H | `steps` (phase 7) |

#### F. Panics

| Behavior | Kind | Home |
|---|---|---|
| A task's formatted panic stops in the task, selecting the frame that panicked; message and location equal the program's hook's | H | `panics` |
| `&'static str`, `unwrap` on `None`, and `expect` on an `Err` give exact messages, at the caller's line past `#[track_caller]` | H | `panics` |
| `panic_any(42)` stops; its message is unavailable, naming the payload type | S | `panics` |
| The stop says tokio will catch the panic; continuing, the program runs on as natively | H | `panics` |
| A panic in `main` stops, then exits 101 | H | `panics` |
| A panic inside the program's own `catch_unwind` stops, and continuing recovers as natively | E | `panics` |
| `resume_unwind` is reported as a re-raise | E | `panics` |
| A panic in `Drop` during unwinding: two stops, then the abort the native run ends with | C | `panics` |
| A panic in `spawn_blocking`, and in a `LocalSet` task, names its task | H | `panics` |
| With `panic = "abort"`: a panic stop, then SIGABRT | H | `panics` (abort) |
| A core of a `panic = "abort"` crash reports the panic as its stop, on the panicking thread | E | `panics` (abort core) |
| The panic filter off: no stop, and every exit equals the native run | S | `panics` |
| The program's own panic hook still runs, and its output is unchanged | E | `panics` |
| A panic while a step waits ends the step at the panic | C | `panics` |
| Go programs keep their exception defaults; Rust's filter is listed and on | H | DAP test; existing Go suites |

#### G. Values and views

| Behavior | Kind | Home |
|---|---|---|
| `JoinHandle`: pending, finished with output, panicked, cancelled, and output taken, each with its task id | H | `values` |
| `Mutex`, `RwLock`, and `Semaphore`: unlocked, held, held with waiters (waiter task ids), and the guarded value | H | `values` |
| `mpsc` bounded and unbounded: queued items, capacity, closed; `oneshot`: empty, sent, closed; `watch`, `Notify`, `broadcast` | H | `values` |
| `Sleep` and `Interval` deadlines against the runtime's clock, on a core too; `Instant` | H | `values` |
| `JoinSet` with its tasks; `task::Id` as its number | H | `values` |
| `Arc` and `Rc` with their counts, `*arc` dereferenced; `UnsafeCell` transparent; `print/x` on atomics | H | `values` |
| A coroutine value prints as its state, with its saved locals, and as stored under `[raw]` | H | `values` |
| `Poll<T>` reads as `Ready(v)` or `Pending` | H | `values` |
| A view that does not bind (a layout moved) shows the value as stored, with the check that failed | S | views' own tests |
| Every view verified by `VIEW:` markers in both builds, live and on cores | H | `values` |
| Listing a frame of tokio's own code never fails on a variant part | S | `steps` (regression for "neither a discriminator nor a tag type") |

#### H. Clients

| Behavior | Kind | Home |
|---|---|---|
| CLI `tasks` renders id, place, state and wait, and thread; `-g` groups by await site with counts; `-t` adds async stacks; `-a` adds the runtime's own | H | one CLI rendering test |
| CLI `task N bt`, and `task N <command>` restoring the selection afterwards | H | CLI |
| Long generic arguments are elided in backtraces, keeping path and method | H | CLI |
| Panic stops print message, location, and task; co-hits print one line each | H | CLI |
| `[step] runtime` in each settings file, with flag and file precedence; a bad value fails `config check` with the nearest valid one | S | `tests/config.rs` |
| DAP: tasks as threads, named `[7] steps::middle — sleeping (thread 4122)`; idle runtime threads hidden; the program's own threads shown; `maxTasks` cut with "N more" | H | `tests/dap/tasks.rs` |
| DAP: async frames with sources, `label` frames at segment changes, `subtle` runtime frames | H | DAP |
| DAP: an async frame's scopes have locals and no registers; `evaluate` and `setVariable` work there | H | DAP |
| DAP: `next`, `stepIn`, and `stepOut` on a task's thread id follow the task | H | DAP |
| DAP: one `stopped` event names the stop's thread; every co-hit thread is marked stopped by its breakpoint | C | DAP |
| DAP: `exceptionInfo` for a panic; filters from every runtime with their defaults | H | DAP |
| DAP: a request naming a task that no longer exists fails with "unknown task", not a crash | S | DAP |
| Web: the tasks panel lists, groups, and selects; stack, variables, and source follow the task; the URL holds it; a shared link reopens it | H | `web/e2e` |
| Web: a tab acting on a stale stop is refused, and refreshes | S | `web/e2e` |

#### I. Lifecycle and robustness

| Behavior | Kind | Home |
|---|---|---|
| Attach to a running server: tasks listed; a request hits a handler breakpoint; detach leaves it serving, and a second request succeeds | H | `server` via `ExternalProcess` |
| Attach while workers are mid-poll: running tasks are `Running` on their threads, or `Unknown` with a reason, never `Blocked` with a stale chain | C | `server` under load |
| Detach with plan breakpoints planted (a step waiting): every trap byte is removed, and the program's output matches the native run | S | `steps` |
| `terminate` and `kill` at any point: no inferior survives (harness check) | S | every scenario |
| The program exits while a task is selected: the selection clears; requests about it fail typed | E | `steps` |
| A stale `StopId` used for a task request fails | S | `workers` |
| Interrupting a busy runtime with `pause` reports every thread's activity | H | `server` under load |

#### J. Bounds on work

These are measured in counted work, never in time:

- A page of tasks reads a bounded number of bytes per listed task, whatever
  the total; this is checked at 100,000 tasks.
- An async backtrace reads a bounded number of bytes per frame.
- A step across a pending await makes a bounded number of stops per
  re-poll of its own task, whatever the number of sibling tasks. Sibling
  hits are counted by the flight recorder and must equal the program's
  count of siblings passing the same resume point.
- Loading the `server` fixture allocates within the existing per-byte bound
  of debug information.

### Property tests

These use the existing `proptest` setup, and only for pure code whose
correctness is a law over its inputs. Each property runs on generated
memory images or values, never on a process.

- **The task-list walker.** It runs on generated sharded lists: random
  shard counts, nodes, ids, and tasks per shard, with optional damage (a
  foreign `owner_id`, a broken back link, a cycle, a held shard mutex, a
  wrong count). Properties:
  - On an undamaged list, every node is listed exactly once.
  - Pages concatenate to the full list whatever the page size, from 1 to
    beyond the total.
  - Each damage is reported as a gap naming its shard, and no damaged node
    is listed as a task.
  - Reads per page never exceed the bound.
  - The walk always terminates.
- **The future walker.** It runs on generated await chains over
  coroutine-shaped type tables: random depths, states, boxed and `dyn`
  links, and leaves. Damage is optional: an out-of-range state, a pointer
  into nothing, a cycle, or a too-deep chain. Properties:
  - An undamaged chain yields exactly its frames, in order.
  - Damage ends the chain with a typed termination at the damaged link, and
    never yields a frame past it.
  - No object repeats.
  - It always terminates within the depth bound.
- **State-bit decoding** is exhaustive rather than random: every
  combination of the six flags, at three reference counts, maps to exactly
  one task state, as the written-down convention table says.
- **`Activation::Object`.**
  - Object activations are never ordered against stack addresses.
  - `same` is an equivalence.
  - `has_returned` follows the object's state and the task's existence.
- **Generic elision in names** keeps every path segment and the method,
  never touches a name with no generics, and gives the same result when
  applied twice.

### Fuzzing

New `cargo-fuzz` targets, each run through `just fuzz` inside
`scripts/contained.sh`. Each is seeded from real fixture memory (checkpoint
cores) and real fixture DWARF. Every target asserts these:

- no panic;
- bounded memory and reads;
- every refusal typed;
- every result internally consistent.

| Target | Input | Extra assertion |
|---|---|---|
| `tokio_tasks` | Arbitrary memory under the real layout bound from the `workers` image | Every listed task passed validation; thread activity names only listed or blocking tasks |
| `future_walk` | Arbitrary memory and a root, with the `shapes` image's coroutine types | Frames' objects lie in readable memory, and states are in range |
| `coroutine_types` | Synthetic DWARF built from the fuzz input with `gimli::write`: variant parts with arbitrary names, discriminants, and members | Normalization accepts only well-formed coroutines and refuses the rest with a reason |
| `dispatch` | Arbitrary bytes as a resume function's entry, with a state member offset | It terminates; every target it claims lies in the function and starts an instruction; it claims nothing for code it could not decode |
| `panic_payload` | Arbitrary payload memory and vtable symbols | Message lengths are capped, text is valid UTF-8, and unknown payloads are refused |

The existing `dap_request` corpus gains seeds that name tasks and async
frames.

### The simulator

The simulator does not run tokio, and this plan adds nothing to it:

- Its corpus is libc-free C with no TLS. Teaching it Rust or a runtime would
  cost more than real fixtures do.
- The phase-0 refactors touch run control: `Activation`, owners, the task
  cursor, co-hit reporting, and per-runtime exception stops. Each must keep
  `just sim` green with its oracles unchanged, and `just sim 600` runs
  before each run-control phase merges.
- A sweep failure there is a regression in neutral code, handled by the
  simulator's own rules.

### Stress and soak

- **Stress.**
  - Every `tests/tokio` scenario runs under `just stress`.
  - `just stress 100 -E 'binary(tokio)'` targets the suite while run
    control changes.
  - One torture scenario runs 64 tasks through one `async fn` on four
    workers: a conditional breakpoint, repeated `next` and `finish` across
    pending awaits, forced migrations, and aborts.
  - It checks that every step ends with `me` unchanged, that every hit is
    counted once by the program's counts, and that the program exits as
    natively.
  - Every wait has a deadline, and the iteration count is bounded.
- **Soak.** One new recipe, `just soak MINUTES`, runs inside the
  memory-capped scope and is not part of `just all`. It runs at the end of
  phases 3, 5, and 7, and before any release that touches tokio support.
  - **Setup.** It attaches uscope to the `server` fixture under a client
    sending steady load, and to `scale` in churn mode.
  - **The loop, until the time is up:**
    - pause;
    - list every task;
    - backtrace a sample of tasks;
    - print a sample of async frames' locals;
    - toggle a conditional breakpoint in the handler;
    - step across an await in a handler;
    - continue.

    Every tenth round it detaches and attaches again.
  - **Checks:**
    - every invariant at every stop;
    - listed tasks are always within the program's live set plus the tasks
      it marks in transition;
    - the server answers every request, and the client's error count stays
      at zero;
    - uscope's resident memory after warm-up stays within a fixed margin of
      its first sample, with no steady growth;
    - no inferior or thread outlives the soak.
  - **Failure.** A failure keeps the flight recordings and the round
    number, and the soak stops at the first failure, as stress does.

### User acceptance

- **Recorded editor sessions.**
  - One VS Code session (`just uat-vscode`) and one nvim-dap session (`just
    uat-nvim`) against `workers` and `steps` are recorded and replayed as
    traffic, under the suite's schema and ordering checks.
  - They cover:
    - tasks as threads;
    - expanding a suspended task's async frames and variables;
    - `next` across a pending await;
    - `stepIn` into an async function;
    - the panic filter and `exceptionInfo`;
    - folded and labelled runtime frames.
- **Web.**
  - `just web-e2e` covers the tasks panel in Chromium and Firefox.
  - `just web-probe` screenshots of each panel state are reviewed when the
    panel changes.
- **A written walkthrough,** in `docs/tokio.md`, is run by a person at the
  end of phases 4, 5, and 7, against `server` in a terminal and in VS Code.
  Its checklist:
  - find a stuck request's task;
  - read why it waits;
  - step through its handler across awaits;
  - catch a handler panic;
  - attach and detach without disturbing clients.

  Anything that surprises the person becomes a test or a doc change.

### Narrow tests, only where logic is intricate

- **The tokio model on fake memory bound to the real `workers` image's
  layout,** as `src/runtime_model/go/tests.rs` does for Go. It covers one
  case for each state-bit class, the blocking queue, a `Pin<Box<F>>` stage,
  a stale `current_task_id`, `CONTEXT` never touched and destroyed, and
  each contract name group missing.
- **Coroutine normalization against the `readelf` record,** for every
  fixture. It is the drift canary for the rustc PRs that may change
  coroutine debuginfo.
- **Resume points against the CPU,** for every fixture's resume functions.
  It is also run on generated dispatch shapes (tables and compare chains),
  assembled with the existing encoder.
- **The panic payload convention** on the pinned toolchain: a test reads a
  known panic's payload and fails when std's layout moves.
- **A fake runtime model on `FakeTrace` whose tasks are suspended futures.**
  It tests, without tokio, the generic run control a step across an await
  is built from: waiting for the task's re-poll, repairing sibling hits, and
  ending when the task finishes.
- **The boundary tests,** with their new rows.

### Sabotage

Each new oracle and invariant has a test showing it catches the lie it
exists for. Each of these must fail `check_tokio_stop` or its oracle:

- a dropped async frame;
- a frame at the wrong await line;
- a missing or extra task;
- a stale value;
- a step ending on a dispatch row;
- a step that changed `me`;
- a hit counted twice;
- a resume-point map off by one state;
- a `readelf` record and a normalization that disagree.

### Written first

Each failure from the experiments becomes a test that fails before its fix,
and is seen to fail:

- tasks are invisible;
- async bodies are all `{async_fn#0}`;
- `break leaf` binds the constructor;
- await-line breakpoints fire only on resumption;
- `next` over a pending await enters tokio;
- `step` into an await takes eight steps;
- `finish` cannot show `Poll<T>`;
- stale stack locals are shown;
- `_task_context` and unnamed parameters are listed;
- task panics do not stop;
- simultaneous hits are lost;
- coroutine values print as `3 {…}`;
- `JoinHandle`, `Arc`, and `UnsafeCell` show nothing useful;
- listing a tokio frame fails on a variant part;
- line breakpoints bind drop glue.

### Not written

- Unit tests of individual field decodings that the checkpoints already
  cover.
- Snapshot tests of whole transcripts. One CLI test covers rendered task
  lists, folded frames, and async frames.
- Assertions on time, or that something did not happen within a window.
- Tests per tokio or rustc release, since both are pinned.
- tokio or a model runtime in the simulator.
- Comparisons with gdb, BugStalker, or hansei.
- Mutation testing.

### Hygiene

The suite follows AGENTS.md:

- every scenario has deadlines;
- a poll that reaches its deadline fails;
- every process is reaped, which the harness verifies;
- fixtures synchronize through handshakes, never sleeps;
- every collecting loop consumes input on each pass;
- heavy runs (fuzzing, soak, stress) go through `scripts/contained.sh`.

A failing tokio scenario keeps its flight recording, as every scenario's
does. The `tests/tokio` binary runs with nextest, one process per test,
since one process can trace only one live session.

## Known limits

- A runtime is found only through a thread that has entered it. A
  current-thread runtime that no thread is in at the stop is invisible.
- Tasks and async frames need DWARF types. `line-tables-only` and stripped
  builds have frames and breakpoints only. Cargo's default release profile has
  no debug information, so a program needs `debug = true` (or `2`) in its
  release profile to be debugged as more than code.
- A suspended async block's captures may have been moved out. They are marked
  as such, never read as certain.
- In optimized builds, a running async function's locals are what its DWARF
  describes. The future's memory is not trusted mid-poll.
- `block_on`'s future is unavailable where optimization leaves it
  undescribed.
- Spawn locations and task names need `tokio_unstable`, and names need
  tracing as well. Without them, a task is named by its future's function.
- Poll counts, durations, and waker histories need tokio-console's
  instrumentation. uscope reads state, not history.
- Time keeps running while the program is stopped, so timeouts fire after a
  long stop. uscope never changes the program to prevent it.
- No function in the program is called, so `Debug` output is only what views
  provide.

## Open questions for implementation

These are decided by experiment in the phase named, with the result recorded
here:

- **Phase 1.** How often the release dispatch compiles to a compare chain
  rather than a table, and whether inlined children's dispatches are found
  inside `raw::poll::<T, S>`.
- **Phase 2.** Whether the `FormatStringPayload` field order holds across the
  nightly pins seen so far. If not, read `string` by matching `Option<String>`
  shapes in the payload, never by guessed offset.
- **Phase 3.** Whether a blocking-pool thread can be told from a `block_on`
  thread by `CONTEXT` alone, through `runtime: EnterRuntime` and `scheduler`.
  If not, the thread's frames' roles decide.
- **Phase 5.** The cost of re-entry breakpoints conditioned on the task when
  thousands of tasks run the same function, measured in stops per step. If it
  is too high, evaluate the task condition from one thread-local read before
  any frame is built.
