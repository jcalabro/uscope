# Assertions

Assertions catch bugs in uscope. They check facts about uscope's own state
that the type system can't express.

## Code map

| Path | What lives there |
| --- | --- |
| `Cargo.toml` | Profiles: where debug assertions and overflow checks are on |
| `src/backend/linux.rs` | `Controller::handle_message`, which every request and wait status passes through |
| `src/backend/linux/invariants.rs` | The controller's invariants, checked after its messages |
| `src/backend/linux/lifecycle.rs` | `detach_inferior`, the correct way to release an attached process |

## Rules

1. **Types first.** If a type can prove a fact, use the type. Assert what
   types can't prove: facts that span structures, facts that span time, and
   facts given up for speed, such as an index in place of a reference. Assert
   more than feels necessary. If a comment says "always" or "never", assert it.

2. **Bugs, not input.** Never assert on DWARF, ELF, core files, tracee memory,
   ptrace or `/proc` results, or client requests. Return an error instead. This
   includes `slice[i]`, `.expect()`, and arithmetic on values from input: use
   `get` and `checked_*`. Fuzzers run with debug assertions on, so a fuzz crash
   on an assertion means an input check is missing.

3. **`debug_assert!` by default.** It runs in test, sim, and fuzz builds and
   costs nothing in release, so expensive checks are fine. Use `assert!` only
   when continuing would damage the tracee or a file uscope is writing. Use
   `const _: () = assert!(...)` for sizes and layouts. Release builds keep
   overflow checks on, so an overflow always panics.

4. **No side effects.** Debug assertions vanish in release. A condition must
   not mutate state, do I/O, or touch the tracee.

5. **Say what broke.** Check one fact per assertion. The message names the fact
   and the values involved: thread, `StopId`, IDs, and addresses with their
   address space. Never include tracee memory or strings, because messages end
   up in bug reports. Release builds have no flight recorder, so an `assert!`
   message must make sense on its own.

```rust
debug_assert!(
    site.installed,
    "lifting the trap at {address} for {owner:?}, but it is not installed",
);
```

## Invariant checks

Give a stateful component a `check_invariants` method and call it under debug
assertions after each change. Prove that it fires with a unit test that
corrupts the state and uses `#[should_panic]`.

The controller's invariants run at the end of `Controller::handle_message`, so
the simulator checks them at every step of every seed. Their cost grows with
the state, so past a few hundred breakpoints, threads, and modules they run
every few messages instead. They must hold even when a ptrace request failed
partway through, as a tracee killed from outside makes one do. Record a
breakpoint site's owner and its bookkeeping one site at a time, so that a
failed edit leaves them in step.

## When an assertion fires

The controller thread dies. Pending requests fail with `RequestCancelled`, and
`shutdown` returns `BackendThreadPanicked`. Clients show both as an internal
error. The kernel then releases the tracees:

- A launched process is killed by `PTRACE_O_EXITKILL`. That's fine.
- An attached process is detached with its traps and debug registers still
  set, so it dies with SIGTRAP at the next trap. That's not acceptable.

The controller must therefore catch the unwind and release an attached process
as `detach_inferior` does. The cleanup lives at the thread boundary, not in an
assertion macro, because any panic can end the controller. It restores only
bytes it can verify: memory still holds uscope's trap and the original byte is
known. Aborts skip it: stack overflow, running out of memory, and a panic
during cleanup.

> TODO: This cleanup doesn't exist yet. Add it around `Controller::run`, along
> with a simulator scenario that panics during an attached session and checks
> that the process's code and debug registers are restored. Also check how a
> panic on the waiter thread reaches the controller.
