# Architecture

uscope separates frontend concurrency from platform-specific process control.

```text
Tokio runtime
  UI and protocol tasks
          |
          | bounded requests and one-shot replies
          v
  platform controller OS thread <--- wait events --- waitpid OS thread
          |
          v
      inferior process
```

`DebuggerHandle` is cloneable and contains no mutable debugger state. Requests
are sent through a bounded Tokio channel, and every request carries a typed
one-shot reply. Debugger events use bounded broadcast subscriptions. A receiver
that falls behind gets an explicit lag error and can recover by requesting a
canonical `StateSnapshot`.

The platform controller is the sole owner of mutable debugger state. On Linux,
it is also the only thread allowed to make parent-side `ptrace` calls. A separate
OS thread blocks in `waitpid` and sends typed wait events back through the same
controller queue. This keeps the controller responsive to requests while the
inferior is running.

Linux process control lives under `src/backend/linux.rs`. Shared handles,
requests, events, snapshots, and stop reasons do not expose Linux-specific
types. Adding another target means selecting another private backend in
`src/backend/mod.rs`; frontend code should not change.

Shutdown owns a reserved queue permit, so termination cannot be starved by
ordinary requests. It stops new work, kills and reaps the inferior, joins the
waiter, and then joins the controller. Dropping the owning `Debugger` also uses
the reserved path as best-effort cleanup.
