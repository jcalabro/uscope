# How uscope debugs Go

Go is the first language whose runtime uscope has to understand. A C
program's threads are its units of work, and its stacks stay where they
are. A Go program's units of work are goroutines, which the runtime moves
between threads, whose stacks it moves in memory, and which mostly sit
parked with no thread holding their registers. This document explains how
uscope handles that: where the knowledge lives, the decisions behind it,
and the traps found on the way. It is for people changing uscope.

What a user sees is in [cli.md](cli.md) and [dap.md](dap.md). The design
record, with the experiments and the survey of other debuggers behind these
decisions, is [plans/go.md](../plans/go.md).

Support covers the pinned toolchain, Go 1.27.1 from `flake.nix`, on Linux
x86-64. That includes PIE and non-PIE executables, optimized and `-N -l`
builds, cgo with gcc and clang, Go libraries (`c-shared`) hosted by C
programs, stripped binaries, attaching, and core dumps.

## What Go demands

- **Goroutines are not threads.** The runtime runs goroutines (G) on OS
  threads (M) through processors (P). A goroutine changes threads whenever
  it blocks, is preempted, or makes a system call, and a thread runs many
  goroutines. At any stop, most goroutines are parked, with only the pc,
  sp, and bp they saved in `g.sched`.
- **Stacks move.** A goroutine starts with a small stack.
  `morestack` → `newstack` → `copystack` replaces it with one twice the
  size elsewhere, and the collector shrinks it again. Any stack address
  remembered across a resume can go stale.
- **A thread has three kinds of stack.** It has the goroutine's own stack,
  the system stack (`g0`) for the scheduler and for `systemstack` work, and
  the signal stack (`gsignal`). cgo runs C on `g0`, and Go called back from
  C on the goroutine's stack. Go's call-frame information cannot describe
  any of these switches.
- **The runtime's own traceback is the ground truth**
  (`runtime/traceback.go`). It ends at functions flagged `TOPFRAME`, stops
  at `SPWRITE` functions, crosses from `g0` to `m.curg` at `systemstack`
  and `morestack`, and treats the caller of `sigpanic` and `asyncPreempt`
  as an exact faulting pc. Those flags live only in `.gopclntab`.
- **Registers are not preserved across calls**, and r14 holds the current
  goroutine only inside Go's internal ABI. Thread-local storage is the
  authority everywhere else.
- **Faults become panics.** A nil dereference raises SIGSEGV, which the
  runtime's handler turns into a panic the program may recover. SIGTRAP is
  fatal to a Go program.
- **The runtime preempts with SIGURG.** sysmon signals a thread whose
  goroutine has run for 10 ms, and time spent stopped in a debugger
  counts toward that.
- **Every release changes something.** Field layouts, status numbers, wait
  reasons, the type descriptor, the function table, and function IDs all
  move. DWARF describes the layouts, but not the conventions around them.

## Where Go lives

Most of uscope does not know Go exists. Go is named in three places:

| Place | What it knows |
|---|---|
| `src/debug_info` | `.gopclntab` (`gopclntab.rs`, `gopclntab/`), Go's DWARF attributes, and the roles of Go's code (`roles.rs`) |
| `src/runtime_model/go` | The runtime's state at one stop: goroutines, threads, stack switches, panics, interfaces |
| `views/go-*.views` | Library types: maps, channels, `time`, `sync`, text, errors |

Everything else speaks of tasks, stack segments, code roles, and language
exceptions. That covers the protocol, the model, run control, the
evaluator, the CLI, and DAP. Two tests in `src/runtime_model/tests.rs` hold
this line. `runtime_model_stays_pure` keeps process control, debug-info
parsing, I/O, clocks, and threads out of the runtime model.
`languages_stay_at_their_seams` keeps Go's runtime names (`"runtime.`,
`allgs`, `goroutine`, `goid`) out of `cli/`, `dap/`, `backend/`, `eval/`,
`unwind.rs`, `protocol.rs`, and `lib.rs`.

The split follows one rule. **A fact that holds for every execution of a
function is static, and belongs to the debug-info provider.** Examples are
that a function switches stacks, or is a wrapper. **A fact about one stop
of one process is dynamic, and belongs to the runtime model.** Examples
are which goroutine a thread runs, or where a parked goroutine stopped.
Unwinding and stepping act on the static facts. They ask the runtime model
only for what needs the program's memory.

### Static facts: code roles

The provider gives each function a `CodeRole` once, at load
(`src/model.rs`). Unwinding and stepping read only roles, never names or
languages. `go_role` in `src/debug_info/roles.rs` assigns them from the
function table's flags and IDs, following the runtime's own traceback.
When several roles apply, the first in this list wins:

1. `SignalTrampoline`: `runtime.sigreturn__sigaction`, Go's
   `sa_restorer`. The interrupted registers are in the kernel's signal
   frame above it.
2. `TrapEntry`: `sigpanic`, `asyncPreempt`, and `debugCallV2`, which the
   runtime injects. Their caller's pc is the instruction that trapped,
   not a return address.
3. `Outermost`: functions flagged `TOPFRAME` (`goexit`, `mstart`,
   `rt0_go`). Unwinding ends here, complete. `sigtramp` is flagged too, but
   unwinding goes on through the signal frame, so it is excluded.
4. `StackSwitch`: functions flagged `SPWRITE`, and the switches the runtime
   knows by ID (`systemstack`, `mcall`, `morestack`, `asmcgocall`,
   `cgocallback`). Only the runtime model can say where they continue.
5. `Wrapper`: code Go generates, such as ABI wrappers and
   `<autogenerated>` code, plus `deferreturn` and the code cgo writes.
   Steps pass through it.
6. `Panic`: `gopanic`. A step goes through it into the deferred functions
   it calls.
7. `Dispatch`: `execute`, which hands the thread to a goroutine.
8. `RuntimeInternal`: what the runtime's traceback hides. That is any
   unexported `runtime.` function, and anything under `internal/runtime/`
   or `runtime/internal/`.
9. `Ordinary`: everything else.

The same roles serve C. glibc's `__restore_rt` is a `SignalTrampoline`, and
`_start` is `Outermost`.

### `.gopclntab`

Go's function table survives stripping, and uscope always reads it. Only
the format of Go 1.20 through 1.27 (magic `0xfffffff1`) is accepted. Any
other magic is refused with the releases that wrote it. Function IDs are
interpreted only for releases whose numbering is written down. Every read
is bounded, and a malformed table is a typed error.

The table supplies:

- names, lines, and inline trees for binaries without DWARF;
- stack-pointer deltas for unwinding (`GoUnwind`);
- the flags behind code roles;
- the prologue analysis that places entry breakpoints.

A stripped Go binary therefore gets named, unwound frames, function and
line breakpoints, and steps. It gets no goroutines or values: those need
DWARF, and are refused with a reason rather than reconstructed from a
guessed layout.

### The runtime contract

Every name the Go model reads is bound against the runtime's own DWARF when
the image loads (`src/runtime_model/go/layout.rs`). Offsets, sizes,
statuses (`runtime._G*`), and wait reasons all come from the binary by
name. Nothing comes from a table of versions.

- **A missing name disables only what needs it.** The feature reports a
  reason that names the field, such as "the runtime has no member
  runtime.g.goid". Other features keep working.
  `a_missing_name_makes_only_what_needs_it_unavailable` checks this.
- **A size that differs is refused, not truncated.** The contract checks
  each field's size as well as its offset.
- **Only conventions are written down.** These include where TLS keeps g,
  which functions switch stacks and how, the meaning of `_Gscan`, how an
  interface holds its value, and which registers carry a hook's arguments.
- **Other releases are read but flagged.** A binary from a release other
  than the verified one (`VERIFIED`, 1.27) is read the same way wherever its
  DWARF binds, and every result carries the gap "goX.Y is unverified". A
  release older than 1.20 is refused.
- **Without a Go producer, nothing is bound.** An image that names
  `runtime.goexit` but has no Go producer in its DWARF is refused as
  stripped.

## Goroutines as tasks

The public model has *tasks*, units of work a language runtime schedules on
OS threads. A task is a scheduling identity, not a stack. It has a number,
a state, the runtime's own words for what it waits on, an optional thread,
where it resumes, where it was created, its entry, its parent, and its
labels. Go's goroutines are the only tasks so far. `RuntimeModel` in
`src/runtime_model/mod.rs` is the seam, and the backend binds one model per
image that carries a runtime (`src/backend/linux/runtimes.rs`). A cgo
program has one model, and a C program hosting a Go library gets one when
the library loads.

### Which goroutine a thread runs

A thread's goroutine is the word at a fixed place in its thread-local
storage:

- An executable linked by Go's own linker keeps it at `fs_base - 8`.
- An externally linked program (cgo) keeps it at `runtime.tlsg` within the
  TLS block, after the C's own thread-locals.
- A `c-shared` library keeps it where the loader wrote the
  `R_X86_64_TPOFF64` GOT slot, which is read from the process.

That g is then interpreted through the thread's `m`:

- If g is `m.g0`, the thread is running the runtime's code on its system
  stack for `m.curg`. It presents as that goroutine, on the `System`
  segment.
- If g is `m.gsignal`, it is running a signal handler for `m.curg`, on the
  `Signal` segment.
- If `curg` is zero, the thread is idle in the scheduler or is a C thread,
  and runs no task.
- A thread executing `runtime.settls` has no valid g yet, and its task is
  reported unknown.

r14 is never read. Every g the model follows is checked against
`runtime.allgs` first, so a corrupted pointer is reported, never read as a
goroutine.

### Listing goroutines

`allgs` can hold millions of entries, most of them dead. A page of tasks
reads `allgs` in a window of at most four entries for each task it may
return, and at least 256. It skips dead goroutines and, unless asked, the
runtime's own. A page can therefore come back short and still have a next
page; the work per page is bounded either way. Each listed task carries a
*locator*, its g's address, so selecting it later at the same stop reads
only that g. A locator is trusted only if the g there still has the same
goroutine id. A corrupted `allglen` is capped at 2²⁴ entries.

Some details of reading each goroutine:

- **Status.** The `_Gscan` bit is masked off, so a goroutine the collector
  is scanning shows the status it returns to.
- **What it waits on.** A waiting goroutine is described by its wait
  reason, from the runtime's own `waitReasonStrings`. Any other is
  described by its status.
- **Runtime goroutines.** A goroutine counts as the runtime's own when it
  began in a `runtime.` function, as the runtime's dump decides, except
  `runtime.main` and a few that run the program's code. Lists leave them
  out unless asked.
- **Profiler labels** are read when `g.labels` binds. A missing field is
  reported once per page, not once per goroutine.

### A parked goroutine's frames

A parked goroutine's frames begin at its `g.sched`. That holds only pc, sp,
and bp. Every other register is unknown, never zero: inventing a full
register set would fill the rest with convincing garbage. Go keeps nothing
in registers across a call, so its frames lose nothing. If the saved sp is
outside the goroutine's own stack bounds, the goroutine is refused as
unreadable rather than unwound from a corrupted record
(`tests/go/corrupted.rs`).

## Unwinding across stacks

The unwinder treats every frame through its code role. For a frame whose
role is `StackSwitch`, it asks the runtime model where to go, through
`RuntimeModel::cross`. The answer is a `Crossing`:

| Crossing | Meaning |
|---|---|
| `Stay` | The frame has not switched yet, or has switched back. It unwinds as usual. |
| `Resume(registers)` | The frame runs elsewhere than its registers say. It unwinds from these registers instead. |
| `Continue(registers)` | The frame never returns. The goroutine it left goes on at the registers it saved. |
| `Outermost` | Nothing lies beyond this frame. |

Each switch is crossed the way the runtime itself crosses it:

| Function | How it is crossed |
|---|---|
| `systemstack` | It returns to its caller on the goroutine's stack, at `curg.sched`'s sp and bp. |
| `morestack`, `mcall` | They never return. The goroutine goes on at `curg.sched`. |
| `gogo` | On the system stack it is an ordinary call. Mid-switch, it is refused. |
| `asmcgocall` | C may call back into Go, which saves `g.sched` anew. So the goroutine and how far below its stack's top it was are read from `asmcgocall`'s own frame on the system stack (0(SP) and 8(SP)). That depth survives a stack move made by the callback. |
| `cgocallback` | Unwinding goes on through the C that called it, at `m.g0.sched.sp`. The runtime's own traceback skips that C instead. |
| `nanotime1`, `vgetrandom1` | A vDSO call keeps the goroutine's sp in r12, which C preserves. |
| `clone` | The new thread's first frame is outermost. The parent goes on as usual. |

Any other stack switch is refused by name ("… switches stacks in a way the
debugger does not follow"); the unwinder never guesses.

Every frame belongs to a `StackSegment`: `Task`, `System`, `Signal`, or the
thread's own. A frame's segment is its callee's, unless the callee switched
stacks or a signal interrupted it. The runtime knows a cgo thread's system
stack only approximately, so segments are not decided from address ranges
alone. Backtraces label each run of frames with its segment. Nothing is
hidden: runtime frames, wrappers, and system-stack segments all appear, and
clients mark them (DAP gives them the `subtle` hint).

## Run control

### Activations relative to the stack's top

Run control remembers activations across resumes: the frame a step began
in, the caller it returns to, the frame a watched local lives in. On a
thread's stack an activation is its canonical frame address. On a
goroutine's stack it is the CFA's *offset below the stack's top*, which a
move preserves (`src/backend/linux/activation.rs`). The stack's bounds are
read again at every stop. Run control never compares raw stack addresses;
the predicates `is_callee_of`, `has_returned`, and `just_returned` are the
only comparisons. This is what keeps a `next` over a call that grows the
stack from running away through every later call.

### Steps belong to goroutines

A step, `finish`, or instruction step records the task it began in
(`StepOwner`). Whether a stopped thread is running that step is decided by
asking the runtime which task the thread runs (`runs_step`), not by
comparing thread ids. When the goroutine resumes on another thread, the
step follows it (`follow_step`). A plan breakpoint hit by another goroutine
running the same function is repaired invisibly, as hits by other threads
always were. A breakpoint, watchpoint, or signal that belongs to another
goroutine still ends the step where it happened; uscope never leaves a step
pending.

Only code on the goroutine's own stack ties a step to the goroutine. Code
on a thread's system stack stays with the thread, whatever goroutine it
serves (`step_task`).

### What a step skips

Source steps pass through code the program's author did not write, judged
by role (`passes_over` in `stepping.rs`). They pass through wrappers, stack
switches, and `gopanic` into the code those call. They also pass through
the runtime's machinery, unless the step began there. A step into a map
assignment or `append` therefore steps over the runtime. A `step` at a
`return` enters the function's deferred calls: open-coded ones are ordinary
calls, and the rest pass through `deferreturn`, a wrapper. A panic stops
`next` and `finish` in the deferred functions it runs.

A range-over-func body is compiled as a function of its own, named
`F-rangeN`. The provider links it to `F` as its *enclosing* function. A
`next` in the body treats each call back into the iterator as a call, and
so stops at the next body line or the line after the loop.

`step goroutine` (`StepKind::IntoNewTask`, `src/backend/linux/new_task.rs`)
steps over a line while it watches `runtime.newproc1`. When the step's own
goroutine starts one, it reads the new goroutine that `newproc1` returns in
rax, waits at its entry, and goes on as a step in to its first statement.

Steps go between Go and C through `runtime.cgocall` and `crosscall2`, whose
targets are in rax and rdi (`RuntimeModel::call_out`).

### Entry breakpoints and the stack check

A Go function begins with a stack check that may call `morestack`.
`morestack` grows the stack and runs the function again from its first
instruction, so a breakpoint on the first instruction would fire twice for
one call. Function breakpoints are placed past the prologue, after the
check, where the line table and the prologue analysis agree
(`EntryProvenance::AnalyzedPrologue`). The panic hook on `gopanic` is
placed past it for the same reason.

### Signals

Go adds a layer to signal policy, between the user's choices and gdb's
defaults (`RuntimeSignals`):

- **SIGSEGV, SIGBUS, and SIGFPE are the runtime's to handle.** By default
  they are delivered without stopping or printing. A fault the program
  recovers is not an event; one it does not recover stops as an
  unrecovered panic, and one the runtime cannot turn into a panic stops as
  a fatal error. `handle SIGSEGV stop` (or DAP's `signals` setting) still
  makes the raw signal stop.
- **SIGURG is deferrable.** A SIGURG that arrives while uscope runs one
  thread alone is held until the thread's next continue; this happens
  during a single step, or while a thread steps over a breakpoint. If it
  were delivered then, the preempted goroutine could park until a stopped
  world restarts, which needs a thread the debugger is holding, so the
  step would never end (`tests/go/preemption.rs`).
  The runtime rechecks preemption whenever the signal arrives, so a late
  one is harmless. A plain continue delivers SIGURG at once, so a program's
  own `signal.Notify(SIGURG)` sees every one (`tests/debugger/signals.rs`).

uscope never changes the program's environment. It sets no
`GODEBUG=asyncpreemptoff`.

### Panics and fatal errors

The runtime reports its exceptions by calling functions uscope plants
internal breakpoints on, only while the user wants those stops
(`src/backend/linux/language_exceptions.rs`). The reports are read from the
functions' register arguments, rax then rbx
(`src/runtime_model/go/exceptions.rs`).

| Hook | Reports |
|---|---|
| `runtime.fatalpanic(msgs *_panic)` | An unrecovered panic, with the chain of panics as `printpanics` prints it, and the panic's value as an expression. |
| `runtime.gopanic(e any)`, past its stack check | Every panic as it begins. Off by default. |
| `runtime.throw`, `runtime.fatal` | A fatal error, such as a deadlock or a concurrent map write. |
| `runtime.fatalsignal` | A signal the runtime cannot turn into a panic, such as a fault in C. |

The message is the one the runtime would print. Floats and complex numbers
are formatted as `strconv` formats them. An error or Stringer of the
program's own has no text when the panic begins, since uscope never calls
the program's methods; the runtime has already converted such values by
the time it reaches `fatalpanic`. The stop selects the frame that panicked,
below `gopanic`, or the faulting frame below `sigpanic`.

An `int3` in the program's own text that uscope did not plant, as
`runtime.Breakpoint()` makes, stops as a program breakpoint and resumes past
it. Its SIGTRAP is never delivered, since SIGTRAP kills a Go program.

### Watching a goroutine's locals

A watched local on a goroutine's stack belongs to the goroutine, and is
placed by its offset below the stack's top
(`src/backend/linux/stack_watches.rs`). While such a watch exists, uscope
plants a breakpoint at `runtime.copystack`, which takes the goroutine to
move in rax. An internal all-stop sets that goroutine's watches aside, so
the copy's own reads and writes never stop the program. When `copystack`
returns, the watches are placed on the new stack. A move uscope cannot
follow ends the watches it may have moved, rather than leave them watching
memory their objects left. Debug registers are armed on every thread, so a
watch follows its goroutine between threads.

## Values

The DWARF provider normalizes Go's conventions at the boundary
(`src/debug_info/dwarf/variables*`):

- **Escaped variables.** Go names a variable it moved to the heap `&x`,
  described as a pointer. It presents as `x`.
- **Results.** `~r0` and named results (`DW_AT_variable_parameter`) are
  grouped with the arguments. After `finish`, the returned values are shown.
- **Visibility.** A local is in scope only after its declaration line. This
  is Go's rule, and it avoids showing garbage for variables not yet
  declared.
- **Compiler temporaries** whose names start with `.` or `#` (`.dict`,
  `.closureptr`, `#yield1`, `#state1`) are hidden from listings, but stay
  reachable by name.
- **Shape-typed generics** resolve their real types through the function's
  dictionary (`DW_AT_go_dict_index`).
- **Closures** show their captured variables (`DW_AT_go_closure_offset`).
- **Composite locations** (`DW_OP_piece`), which optimized Go uses for
  every string, slice, and interface in registers, are read, and partial
  values are marked partial.
- **Interfaces** show their dynamic type. The model finds each module's
  type table through `runtime.firstmoduledata`, not an ELF symbol, so
  stripped symbol tables still work.

Library types are views, verified against the pinned toolchain by
fixtures: maps (swiss tables) and channels in `go-runtime.views`;
`time.Duration`, `time.Time`, and `time.Location`; `sync.Mutex`,
`RWMutex`, and the `atomic` types; `[]byte`, `strings.Builder`, and
`bytes.Buffer` as text; and error chains.

**A map is never shown wrong while it grows.** Go's maps grow by splitting
a full table into two and installing the halves in the directory one store
at a time. The map view declares its count from `used`, so while the
directory holds neither half whole, listing it generates fewer entries than
it declares. The view machinery then refuses it instead of showing a short
map (`tests/go/maps.rs` checks every instruction of the split).

**A pointer below a frame's stack pointer is stale.** On a goroutine's
stack, memory below a frame's sp belongs to its callees. Go leaves dead
slots there that hold pre-copy addresses (go#75124), so uscope reads
nothing there from that frame and says why.

**uscope never calls the program's functions.** That rules out `String()`
and `Error()`; views and the runtime's own messages stand in for them.
Calling through `runtime.debugCallV2` would need other threads running
during the call, which breaks the all-stop model.

## Pitfalls

These are the easy ways to get Go wrong.

- **Never trust a stack address across a resume.** A CFA, a watched
  address, or a saved activation may belong to a stack that has since
  moved. Use `Activation` and `StackPosition`.
- **Never trust r14.** It holds g only inside Go's internal ABI. C code,
  ABI0 wrappers, and stack switches all leave it meaningless.
- **Never follow a g without checking `allgs`.** Program memory can be
  corrupted, and a debugger that follows a bad g pointer shows convincing
  nonsense.
- **A thread in `runtime.settls` or `clone` has no valid g.** A new thread
  starts with its m's TLS block, whose g slot is still empty.
- **ABI wrappers share their function's DWARF name.** `break
  runtime.newstack` would otherwise resolve to two locations, with the
  wrapper first. Wrappers are recognized by role, and never chosen.
- **Entry breakpoints must be past the stack check.** Otherwise every call
  that grows the stack stops twice.
- **SIGTRAP is fatal to Go.** No debugger trap may ever reach the program.
- **Holding threads can deadlock the runtime.** A goroutine preempted
  while its siblings are held may wait for one of them to restart the
  world. This is why SIGURG is deferrable.
- **Time stopped counts as running.** After a long stop, sysmon preempts
  everything at once, so steps must expect SIGURG.
- **Optimized Go has DWARF gaps** (go#67130, go#72053). Values the compiler
  did not describe are unavailable. They are never reconstructed.
- **Panic messages may not exist yet.** At `gopanic` the runtime has not
  converted an error or Stringer to text, and uscope cannot call the
  method. Such a stop is named by the value's type.

Tests have pitfalls of their own, because Go's scheduling is less
deterministic than it looks. Each of these passed idle and failed under
`just stress`:

- **Goroutine ids are not predictable.** The runtime hands each P ids in
  batches of 16, so its own goroutines are not reliably 2 to 6. Match them
  by function, never by id.
- **`Done()` is not "parked".** A goroutine that signals a `WaitGroup` and
  then blocks may still be running between the two. A fixture must observe
  every goroutine parked in its own `runtime.Stack(buf, true)` dump, which
  stops the world. The `/sched/goroutines/*` metrics are documented as
  approximate, so they prove nothing.
- **Hits can land in other threads' stops.** When a step ends, another
  thread may have hit a breakpoint at the same stop. Its hit is in that
  thread's `ThreadState::Stopped`, not in the stop's reason.
- **Terminating can stop on the way out.** `terminate` resumes the program
  to receive SIGTERM. A program that signals itself afterwards may stop
  first, as documented. A recorded session should press Stop only where
  the program can do nothing but end.
- **Cores of Go programs are huge.** Go reserves gigabytes of `PROT_NONE`
  heap, so the build records fixture cores with gdb's `gcore` under a
  `coredump_filter` that skips it.

## Testing

Go tests compare uscope with what the program itself reports, never with
gdb or Delve. gdb unwinds into garbage at Go's stack switches, and Delve
has known bugs that agreeing with it would import.

- **The program reports its own truth.** Fixtures print tab-separated
  `TRUTH` lines at each checkpoint, from the runtime's own goroutine dump,
  `runtime.Callers`, and `syscall.Gettid()`, then call `main.reached`
  (`tests/go/truth.rs`). Because the runtime under inspection computes
  them, they stay right when the pin moves.
- **Every stop is checked.** Scenarios built with `checked` run
  `check_go_stop` after every stop (`tests/go/invariants.rs`). Every
  thread must be accounted for. Every backtrace, of every thread and
  parked goroutine, must end properly, name every frame in the runtime's
  image, and change stacks only where the runtime switches.
- **Each oracle is sabotaged.** `the_truth_fails_on_the_lies_it_looks_for`
  and `the_checks_fail_on_the_faults_they_look_for` show the oracles catch
  a dropped frame, a missing goroutine, a wrong value, and a step that
  changed goroutine.
- **Failing programs end as they do alone.** Each case in
  `tests/go/failing.rs` runs once natively. Under the debugger it must exit
  the same way, with the same message.
- **Walks are written in the source.** `// WALK:` markers in fixtures name
  the lines a `next` sequence visits, and tests find lines by marker, never
  by number.
- **The model has narrow tests on a real layout.**
  `src/runtime_model/go/tests.rs` binds the model to a real binary's DWARF
  and static data, and writes fake goroutines and threads into memory. A
  fake runtime model on `FakeTrace` in `src/backend/linux/tests.rs` tests
  following a task across threads without Go.
- **Load and run under pressure.** `tests/go/torture.rs` runs eight
  workers through one function under a conditional breakpoint, with steps,
  `finish`, SIGURG, and the collector running throughout. Every step must
  end in its own goroutine, and no hit may be lost or counted twice.
  `tests/go/gofmt.rs` debugs gofmt built from the pinned GOROOT, and
  `tests/go/scale.rs` lists 100,000 goroutines with the work per page
  measured in counted memory reads, not time.
- **Fixtures force what they need instead of hoping.** They use
  `LockOSThread`, deep recursion to grow a stack, endless `runtime.GC()` so
  the runtime keeps preempting, and SIGURG sent by the program itself, and
  tests set `GOMAXPROCS`. They synchronize through channels and their own
  goroutine dumps, never by sleeping.

The build matrix (`scripts/build-test-programs.sh`) varies only what can
change behavior. Every fixture is built with and without `-N -l`. The rest
cover PIE and non-PIE, cgo with gcc and clang, `c-shared`, `-s -w`, and
`-trimpath` where they matter. The simulator has no Go.

## Moving the pin

The pin is a change of its own:

1. Update the Go version and hash in `flake.nix`, then rebuild with
   `just build-test-programs`.
2. Update `VERIFIED` in `src/runtime_model/go/mod.rs`.
3. Run the Go suites. A field the new release renamed shows up as a
   missing-name reason, not a wrong value.
4. Check each `views/go-*.views` against its fixture, and update its
   "Verified against" line.
5. Re-check the function-table format and function-ID numbering in
   `src/debug_info/gopclntab.rs`. A new magic number must be added
   deliberately.

## Known limits

- A parked goroutine has only pc, sp, and bp, so its innermost runtime
  frames' other registers are unknown.
- Goroutines and values need DWARF. A stripped binary has named frames,
  breakpoints, and steps only.
- Goroutines cannot be frozen or resumed individually. Neither the runtime
  nor any debugger supports that (go#31132).
- The program's functions are never called, so a value is shown through its
  `String()` or `Error()` method only if a view or the runtime supplies the
  text.
- DAP has no `stepInTargets` yet, so `step goroutine` is a CLI command only.
