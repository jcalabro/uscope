# Profiling with uscope

`uscope record` samples where a program spends its CPU time, and `uscope
report` summarizes what it recorded. The profiler works from outside the
program: the kernel's perf events take every sample, so nothing is added
to the program and nothing is stopped. This document says what a user can
do and what the profiles mean. The design, with the measurements behind
it, is [plans/profilers.html](../plans/profilers.html).

## Recording

```sh
uscope record -o app.usprof -- ./app --its --arguments
uscope report app.usprof          # the most expensive functions
uscope report --tree app.usprof   # the call tree
```

`record` runs the program to its exit and keeps its exit status as its own.
It follows every thread and every child process the program starts. A
preset chooses how much to record:

| Preset | Samples a second | Stack copied | For |
| --- | --- | --- | --- |
| `--preset prod` | 49 | 512 bytes | production: a fraction of a percent of one CPU |
| `--preset dev` (the default) | 999 | 16 KiB | development: unwinds code without frame pointers |
| `--preset max` | 9999 | 16 KiB | short runs that need every detail |

`--rate HZ` and `--stack-bytes N` override a preset's numbers. `--function
SYMBOL` samples each call of a function instead of CPU time, which counts
calls exactly; the program then runs without address randomization.

The kernel must let the user sample their own processes:
`/proc/sys/kernel/perf_event_paranoid` must be 2 or less, as it is by
default. Rings that would lock more memory than the kernel allows the user
are made smaller, and the profile records the size it got.

## What a stack means

Every stack in a profile is exact as far as it goes, and says why when it
stops short of the program's outermost frame (`_start`, or a thread's
`clone3`). Within the stack copy, uscope unwinds by each module's
call-frame information, which needs no frame pointers. Beyond it, uscope
follows the kernel's walk of the frame-pointer chain, but only from a frame
whose call-frame information proves it keeps its frame pointer, and only
through callers that prove the same. A stack that cannot be continued ends
with one of these:

| Ending | Meaning |
| --- | --- |
| `[stack copy exhausted]` | The stack was deeper than its copy, and no frame on it kept a frame pointer to continue from. A larger `--stack-bytes`, or building with frame pointers, reaches further. |
| `[frame pointer not kept]` | The chain passed through a function built without frame pointers, such as most of the C and Rust standard libraries, so its callers are unknown. |
| `[no unwind information]` | No module describes how to unwind the code. |
| `[unmapped return address]` | A return address no module covers, as in JIT-compiled code. |
| `[frame chain ended]` | The kernel's walk ended at a frame that is not outermost. |
| `[invalid caller]` | Unwinding found a caller that cannot be one. |
| `[depth limit]` | The stack was deeper than the deepest recorded. |

Plain frame-pointer walking, as most profilers do, is wrong wherever a
function without frame pointers sits between two that have them: the stack
looks complete and skips callers. uscope never shows such a stack.

Functions are named with their scopes (`app::server::handle`), and each
function inlined into a frame is a frame of its own.

## Async tasks

A thread running an async runtime switches between tasks, so its native
stacks bury each task under the runtime's scheduler. Reports show logical
stacks instead: a task's frames begin under the frame where the runtime
dispatches it, rooted by the task's outermost function of its own:

```
<tokio::runtime::task::harness::Harness<app::serve::{closure#0}, …>>::poll
  [task] app::serve::{closure#0}
    app::serve::{closure#0}
      app::handle::{closure#0}
        app::decode
```

The runtime's machinery between the dispatch and the task goes, and so do
frames that only forward a call, such as `Pin<Box<F>>`'s `poll`. Samples
of the scheduler itself keep their frames. A goroutine already runs on a
stack of its own, which begins at `goexit`, so a Go stack only loses its
wrappers. uscope knows a runtime's code by the roles the debugger gives it,
so there are no name patterns to configure. `report --native` shows the
frames as the threads ran them.

A task's logical stack needs the frames from its dispatch inward. With
`--preset prod`, the copy of 512 bytes rarely reaches them in code without
frame pointers, so build async Rust with `-C force-frame-pointers=yes` to
profile it in production.

## Profile files

A profile is a file of sealed parts followed by an index of them. A file
cut short still reads: `report` says so and shows every part that was
complete. `uscope record` writes its profile when the program exits. The
format is versioned, and uscope says when it cannot read a file's version.
