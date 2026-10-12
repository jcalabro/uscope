# Process control

The Linux backend controls each tracee independently while the public debugger
presents one all-stop process.

## Thread ownership

The controller runs on the OS thread that created the tracee. Every ptrace call
must run there. A waiter thread may block in `waitpid` and send the resulting
status to the controller; it must never call ptrace.

Tokio coordinates clients and message passing. It does not own blocking native
process control.

## Stops

Classify raw wait statuses before publishing events. Track each live tracee,
its stopped state, and its pending signal. Publish a public stop only when all
live tracees are known to be stopped.

Run-control requests have two distinct results:

1. The request is accepted or rejected.
2. The resumed program later stops or exits.

The acknowledgement comes first. Never make a client wait for the later stop
before acknowledging the request.

Every stopped-state mutation carries a `StopId`. Compare it with the current
stop before changing state. This protects a client whose view lags behind
another client or a rapid sequence of stops.

## Signals

Keep pending signals per tracee. A signal observed while gathering an all-stop
state must stay attached to the thread that received it. Resume policy decides
whether to deliver or suppress it.

## Software breakpoints

A software breakpoint site belongs to the process address space. Physical
installation is separate from the user breakpoints and execution plans that
own the site.

Memory reads shown to users must contain the program byte, not the installed
trap byte. When several threads meet one site, repair them one at a time while
their siblings remain stopped.

## Testing control changes

Cover deterministic reducers or classifiers directly when they contain real
branching logic. Cover public behavior through `tests/support::Scenario` and a
synchronized native fixture. Test cleanup, cancellation, event ordering,
state consistency, and the absence of surviving inferiors.

Run `just stress` and `just sim 600` before merging lifecycle, run-control,
attach, or concurrency work.
