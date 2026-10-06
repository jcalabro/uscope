# Following Fork Children

A program that forks hands its child the parent's code, breakpoints
included. Until now the debugger scrubbed the inherited traps and released
the child to run on its own. Following a child means debugging it too: in
DAP terms, the adapter asks the client to start a child session
(`startDebugging`), which attaches to the child, sets its own breakpoints,
and debugs it beside the parent's session.

## 1. Goals and non-goals

- **One session per process.** A child session is an ordinary attach
  session in its own adapter process, as VS Code and nvim-dap start them.
  The public debugger stays all-stop and single-process; nothing in the
  request/event protocol learns about several inferiors.
- **The child runs no instruction unobserved.** From its fork until its own
  session has attached and configured it, the child executes nothing, so a
  breakpoint at the first line after `fork()` is hit in the child.
- **Nothing is left behind.** A child no session takes runs on, released,
  never stopped forever by a debugger that gave up on it.
- **Opt-in.** `followForks` in a launch or attach configuration. Without
  it, and with clients that cannot start child sessions, children are
  released as before.

Out of scope:

- `vfork` and `posix_spawn` children, which share the parent's memory until
  they exec, and so cannot be scrubbed or handed to another tracer. They
  stay untraced, as before.
- Children that exec another program: their session stops at the exec,
  which it cannot follow (`Exec { followed: false }`), as any session does.
- The CLI, whose sessions are single and synchronous. It releases children.
- A child's output after its parent's session ends. A launched program's
  output reaches the debug console through pipes the parent's adapter
  reads; once that adapter exits, a child still writing to them gets
  SIGPIPE. Programs whose children outlive them should use a terminal.

## 2. Handing a child over

Only one tracer can trace a process, and the child's session runs in
another process, so the parent's debugger must release the child without
letting it run. Linux's way to keep an untraced process from running is a
job-control stop, which any later tracer, or `kill -CONT`, can end.

Probed on Linux (7.2), with the child forked by a launched (`TRACEME`) or a
seized parent:

1. At the child's first stop, the parent's debugger scrubs the inherited
   traps, sends the child `SIGSTOP` with `tgkill`, and detaches it without
   a signal. The child dequeues `SIGSTOP` before running an instruction and
   stops: `/proc` shows `T` and no tracer. Its parent gets `SIGCHLD` with
   `CLD_STOPPED`.
2. The child's session seizes it and interrupts it. The seize turns the
   job-control stop into a ptrace-stop, reported once as
   `PTRACE_EVENT_STOP` with `SIGSTOP`; the interrupt waits for the next
   resume (K-INT-1).
3. Still in that stop, the session sends `SIGCONT` with `kill`. That ends
   the job-control stop, and the parent gets `SIGCHLD` with
   `CLD_CONTINUED`. Resumed, the child first stops for the waiting
   interrupt (`PTRACE_EVENT_STOP` with `SIGTRAP`), then for the `SIGCONT`
   itself, a signal-delivery-stop with `SI_USER` from the session's
   process, which the session suppresses. Then it runs.
4. Without the `SIGCONT`, the child resumed by ptrace runs while its
   process is still job-control stopped, and a later detach stops it again.

A job-control stop is visible to the child's parent only through
`waitpid(WUNTRACED)` and the two `SIGCHLD`s, both of which a stop and
continue from a shell would produce too. Alternatives were worse: parking
the child in a loop of its own code burns a CPU, corrupts its code for any
debugger but uscope, and leaves it spinning forever if no session comes.

The child's identity is its process id and start time, which a session
checks before it seizes and again after, as attach already does: a child
that died and whose id was reused is refused, never continued.

## 3. Debugger API

- `DebuggerHandle::hold_forks()` returns the receiver of the children the
  session holds, `HeldChildren`, in the order they were held. Until it is
  called, children are released as before. A child held is sent as a
  `HeldChild`: its parent's process id and a `HeldProcess { process_id,
  start_time }`. A channel each session owns, rather than an event, so that
  no child is lost to a lagging subscriber.
- A `HeldChild` owns the child's hold. `hand_over()` gives it to whoever
  attaches; `release()`, or dropping it, lets the child run. Dropping the
  receiver releases the children not yet received, and those forked after.
- `Debugger::attach_held(held)` and `attach_held_with_executable` attach as
  `attach` does, check the identity, and end the job-control stop as in
  section 2. `Request::Attach` carries whether the target is held.
- `uscope::release_held(&held)` lets a held child run when no session will
  take it, and `uscope::still_held(&held)` says whether one still waits:
  untraced, and stopped or about to take its SIGSTOP. Both check the
  identity first, and release signals through a pidfd opened before the
  check, so a process that ended in between is never signalled.

**Controller.** A fork child's bookkeeping records where it came from, so
that it names the parent's process even when it first stops after the
parent ended. At the first stop, after the scrub, a child is held when a
receiver is open and the session is not shutting down: a shutdown has no
client left to adopt it, so it is released. Holding fails as releasing
does, by killing a child that cannot be cleaned. The start time is read
while the child is still traced, so its id cannot be reused in between; a
child whose start time cannot be read is released, since no session could
prove it attached to it.

**Attaching a held process.** Once the attach stop is coherent, the
controller sends `SIGCONT` and remembers it, as it remembers the
termination signal it sends; the `SIGCONT` delivery stop from the
controller's own process is suppressed whatever the signal's policy.

**Detaching before the child ran.** The `SIGCONT` is then still queued, and
the program would receive it untraced. The detach first resumes the thread
to take it, as it does for queued traps, and suppresses it. A child held
inside glibc's `fork` blocks every signal but SIGKILL and SIGSTOP, so the
detach unblocks SIGCONT for that resume (`PTRACE_SETSIGMASK`) and restores
the mask once it is taken. A resume that drains a signal while the session
detaches ends the stop the session published.

## 4. DAP

- `followForks` (launch and attach, default `false`). With a client that
  set `supportsStartDebuggingRequest`, the session holds children; with
  one that did not, it says once that children run on their own.
- For each child held, the session sends `startDebugging` from a task of
  its own, so the parent's session keeps serving its client, with an attach
  configuration: `pid`, `held: { startTime }`, a name naming the child, and
  the parent's `type`, `followForks`, `sourceMap`, `viewFiles`,
  `disassemblySyntax`, `signals`, `cwd`, and attach `program`. nvim-dap
  needs `type` to start the child's adapter; VS Code sets its own. The debug
  console says the child is debugged in a session of its own.
- VS Code answers `startDebugging` once the child's session has attached;
  nvim-dap as soon as it starts it. A failure answer, or none within 60
  seconds, releases the child and says so. After a success answer, or a
  connection closed meanwhile, the session started may still be attaching,
  so the follow task waits up to 60 seconds more while `still_held` says
  the child waits, then releases it and says so, which leaves a child a
  session took alone. `USCOPE_ADOPTION_TIMEOUT`, in milliseconds,
  shortens that wait for tests.
- A child session attaches with `Debugger::attach_held`. If that fails, it
  releases the child before answering, so a child no session took never
  waits on a client that gave up. After `configurationDone` it continues
  the child, unless `stopOnEntry` asks to stay.
- Ending the parent's session drops its receiver and waits for its follow
  tasks, which answer or release every child it held.
- Yama's `ptrace_scope` 1 or more refuses the child session's attach, since
  its adapter is not the child's ancestor; `docs/dap.md` says what to do.

## 5. Simulator

The simulator runs the real controller of every session.

- **Kernel.** Job-control stops of untraced processes (K-STOP-1), seizing a
  stopped process (K-STOP-2), and `SIGCONT` (K-STOP-3), each probed by a
  dual-run test, with `SIGCHLD`'s `CLD_STOPPED` and `CLD_CONTINUED`. Every
  traced thread records which tracer traces it; the kernel serves one
  tracer's request at a time, which reaches only that tracer's tracees, its
  waits report only them, and its exit releases only them. Processes have
  start times.
- **World.** Seeds whose program forks follow forks half the time: the
  client holds children, and each child held is adopted by a session of its
  own, or released as `release_held` would. An adopting session is a real
  controller with its own tracer id and waiter, driven by a small client
  that attaches with `held`, continues, pauses, or waits, and shuts down at
  a stop or while the child runs, which detaches it. Now and then the first
  session stops taking children. Actions name their session.
- **Oracles.** *Holding*: a held child is untraced, stopped or about to
  take its SIGSTOP, holds no byte the debugger planted, and has run no
  instruction, until a session seizes it. *Transparency*: no `SIGCONT`
  reaches the program, untraced or passed on by a session, and an adopted
  child's parent sees the same result as ever. All-stop, code integrity,
  site ownership, and clean exit apply to every session's controller. Each
  new oracle has a sabotage test.
- **Marks** for a child held, held after its parent ended, released instead
  of held, released by the world, adopted, its `SIGCONT` suppressed, and
  detached before it ever ran.

## 6. Tests

- Controller units, with the fake edge: holding at the first stop in
  either order with the fork event, after the parent ended, release
  instead during shutdown or with no receiver, attaching a held process
  with its `SIGCONT` suppressed, and detaching before it ran, with the
  mask unblocked and restored and the published stop ended.
- Debugger scenarios (`tests/debugger/forks.rs`, `attach-fork.c`, whose
  child exits differently if it ever receives SIGCONT): a held child
  attached with `attach_held`, stopping at its own breakpoint and exiting
  as it should; a held child detached at once running on unaware; a stale
  identity refused, and a released child running on.
- DAP scenarios: the harness answers `startDebugging` by starting a second
  adapter with the configuration and driving it, as VS Code does; a
  refused `startDebugging` releases the child; a client without the
  capability is told children run on their own. Recorded VS Code and
  nvim-dap traffic of a followed child replays.
- VS Code's and nvim-dap's UATs follow a child into a session of its own,
  stop in it at a breakpoint, and end both sessions.
