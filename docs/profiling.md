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

| Preset | Samples a second | Stack copied | Off-CPU | Budget | For |
| --- | --- | --- | --- | --- | --- |
| `--preset prod` | 49 | auto | off | 1% of a CPU | production: a fraction of a percent of one CPU |
| `--preset dev` (the default) | 999 | 16 KiB | on | none | development: unwinds code without frame pointers |
| `--preset max` | 9999 | 16 KiB | on | none | short runs that need every detail |

In auto mode a sample copies 512 bytes of stack when the frame-pointer
chain carries every stack through the program, and 16 KiB otherwise. Before
the program runs, uscope decodes every call instruction of a statically
linked program and asks its call-frame information whether the calling
frame keeps the chain. A program that loads libraries gets the deep copy,
as its libraries are known only once it runs. The profile's `stack copy`
setting says what auto mode chose, and why, naming the first function that
calls without keeping the chain.

`--rate HZ` and `--stack-bytes N` override a preset's numbers. `--function
SYMBOL` samples each call of a function instead of CPU time, which counts
calls exactly; the program then runs without address randomization. A Go
function's first instructions may run more than once a call, when it grows
its stack or the runtime preempts it, so its calls are counted at its stack
check's branch, which runs once a call without branching. The profile
counts each repeat it sets aside, and a function the runtime may restart at
its entry is refused.

The kernel must let the user sample their own processes:
`/proc/sys/kernel/perf_event_paranoid` must be 2 or less, as it is by
default. Rings that would lock more memory than the kernel allows the user
are made smaller, down to the size that holds two samples, and the profile
records the size it got. A sample that finds its ring full is lost, and
the profile counts every one: the count comes from the kernel's events,
which count losses a ring never reports.

## What recording costs

A report says how much CPU time uscope spent recording, and how much of
that was reading the program's debug information, which happens once per
file:

```
Recording took 0.012 s of uscope's CPU time, 0.43% of one CPU, 0.001 s of it reading debug information.
```

A budget holds that cost down. Each second, uscope measures what the
second's sampling cost it and chooses how much of the next second to
sample: all of it while that fits, and otherwise one span, at a random
offset so it cannot keep step with a program that works in periodic
bursts. Between spans every event is off, so the program pays nothing for
sampling either. The first second is never judged, as it builds what
unwinding reuses. A pause is a gap in the profile: no sample, count, or
off-CPU interval comes from one, the timeline shades it, and the report
says how much was paused and how often the share changed:

```
Sampling paused for 1.389 s of the 3.453 s recorded, to hold uscope to 5% of a CPU; the share sampled changed once.
```

`--budget PERCENT` sets the budget as a share of one CPU, and `--budget 0`
lifts it. A recording without one that uses more than 5% of a CPU says
that `--preset prod` would hold it to 1%.

## Time off the CPU

With off-CPU recording on, the kernel reports each time a thread leaves its
CPU and each time it comes back. Each interval between the two is exact,
and says whether the thread blocked, as in a sleep, a lock, or a read, or
was preempted while it could still run, as when more threads want a CPU
than there are. `report` totals them:

```
Threads were off their CPUs for 0.307 s: 0.205 s blocked, 0.101 s preempted.
```

An interval has no stack: seeing where a thread blocked needs the kernel's
own stacks, which an unprivileged user cannot sample. An interval still
open when the recording ends runs to its end, and one that began before
the recording, which an attach meets, is left out, as when it began is
unknown. `--off-cpu on|off` overrides the preset; a program that switches
threads very often pays a little for each switch reported.

## Recording a running program

```sh
uscope record -o app.usprof --pid 1234 --duration 30
```

`--pid` records a process that is already running, without stopping it or
changing what it sees: no ptrace, no breakpoints, only perf events. The
recording follows every thread the process has and every thread it starts,
and it ends when the process exits, after `--duration` seconds, or at
Ctrl-C, when the process goes on as it was. The program is read through
`/proc/PID/root`, so a process in a container works, and so does one whose
executable was deleted or replaced after it started, which is read from
its mapping.

An attach takes one event for each thread on each CPU, which a process
with many threads on a machine with many CPUs multiplies quickly. uscope
refuses an attach that needs more than 16384 events, and says how many it
needed.

A report says how much of the program's CPU time its samples cover:

```
The samples cover 81% of the 4.170 s of CPU time the program ran.
```

This holds for a program uscope starts too. The kernel counts the time
exactly, but each thread's event on each CPU
only takes a sample once a whole period has run there, so time a thread
spends on a CPU in pieces shorter than a period may go unsampled. At low
rates, on a program whose threads move between many CPUs, the samples can
cover much less than the program ran, and they favor threads that stay on
one CPU longer. A higher rate covers more.

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
| `[frames before a stack switch not captured]` | A runtime switched stacks, as Go does to run its scheduler, and neither the chain nor the copy reaches the stack it switched from. |

Plain frame-pointer walking, as most profilers do, is wrong wherever a
function without frame pointers sits between two that have them: the stack
looks complete and skips callers. uscope never shows such a stack.

A signal handler's stack goes on through the frame the signal interrupted,
from the registers the kernel saved beside the handler's frames. Code in
the vDSO, which no file holds, unwinds by the vDSO's own call-frame
information: the kernel maps one image into every process. The vDSO names
only its entry points, so its own functions show as `[vdso]+0x…`.

Go runs some of its work on a thread's system stack. Where a goroutine
switched there with `systemstack`, which keeps the frame pointer, its
stack goes on into the goroutine's frames:

```
runtime.stopTheWorldWithSema
runtime.gcStart.func2
runtime.systemstack
runtime.gcStart
runtime.GC
main.outer
…
runtime.goexit
```

The scheduler's own work, which `mcall` and `morestack` start after a
goroutine gives up its thread, ends at the switch.

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

## Exploring a profile in the browser

```sh
uscope web app.usprof
```

`uscope web` serves a profile when its argument is one, and prints a link
that opens it. The page has three views of the same query:

- **Flame graph**: callers on top and callees below, each frame as wide as
  its samples. Clicking a frame zooms to it, and the search box highlights
  matching frames and says what share of the samples they hold.
- **Call tree**: the same tree as rows, opened down its heaviest path.
- **Functions**: each function's own and total cost. Selecting one shows
  its callers and its callees, merged from every place it is called.

The timeline above the views has a lane for each thread, showing when its
samples were taken over bands of its time off the CPU: grey where it
blocked and amber where it was preempted, each as strong as the share of
that slice it was off. A thread's name shows its totals. Dragging across it selects a time range, and choosing
a thread's name selects that thread. A selected function can be focused
(only samples through it, rooted at it), merged into its callers, dropped
with every sample through it, or have its recursion collapsed. These
transforms apply in order and show as chips that remove them. The view
can be inverted to show callers, switched from async stacks to native
ones, and grouped by function, line, or instruction. The footer always
shows samples lost, throttled, and truncated.

Everything the page shows is in its address, so a reload or a shared link
opens the same view of the same profile.

The picker's **Profile** tab records a program from the page. Its profile
opens at once and fills in while the program runs: first within a quarter
second, then at most every two seconds. **Stop** ends the recording and
kills the program, as does stopping the server. **Save** downloads any
profile as a file that `uscope report` and `uscope web` read.

## Profile files

A profile is a file of sealed parts followed by an index of them. A file
cut short still reads: `report` says so and shows every part that was
complete. `uscope record` writes its profile when the program exits. The
format is versioned, and uscope says when it cannot read a file's version.
