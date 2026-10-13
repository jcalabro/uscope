# Less Unsafe Code, Checked by Miri

uscope denies unsafe code, but about seventeen files opt out. Most of
them do so to make a system call that a safe wrapper could make, and the
rest have no test that would notice undefined behavior. This plan does
two things:

1. Remove every unsafe call site that a safe wrapper or a small redesign
   can replace, without new bugs or measurable slowdowns. What remains
   lives in a few modules, each guarding one boundary.
2. Run what remains under Miri, through `just miri`, in a loop that
   takes seconds to a minute.

Work happens on the `unsafe` branch, in the `../uscope-unsafe` worktree.

## 1. Rules

- **A safe wrapper beats our own unsafe block.** rustix 1.1.5 is already
  in the graph (clap's `terminal_size` brings it), so a direct dependency
  costs no compile time. nix stays where it already works.
- **No redesign only to lower a count.** A site whose removal changes how
  run control waits or stops stays, and says why.
- **Each step is one commit**, so a regression bisects to it.
- **Behavior first.** A step that could change what a debuggee or user
  sees starts with a test that fails if it does.
- **Measure hot paths.** The probe emit path and `Ring::drain` are
  benchmarked before and after their steps.
- Lifecycle and launch changes (steps 3, 4, 6) pass `just stress` and
  `just sim 600` before merging.

## 2. Inventory

Counts are lines that open an unsafe block, impl, fn, or allow.

| File | Sites | What | Plan |
|---|---|---|---|
| `src/backend/linux/tls/thread_db.rs` | 30 | libthread_db FFI and its callbacks | Keep. Miri tests for the callbacks (step 8). |
| `sdk/rust-probes/src/probes.rs` | 17 | Ring allocation, raw record writes, `&*ring`, section walk, libc externs | Shrink to allocation and the section walk (step 7). |
| `src/backend/linux/perf/sys.rs` | 16 | `perf_event_open`, ioctl, mmap, read, poll, sysconf, rdtsc | Safe read, poll, page size (step 2). Keep the rest. Miri test for `drain` (step 5). |
| `src/timings/alloc.rs` | 12 | `GlobalAlloc`, mimalloc option | Keep. Miri runs its tests (step 1). |
| `src/timings/tests.rs` | 10 | Tests driving the allocator | Keep; they are the Miri tests. |
| `tests/support/memory_cap.rs` | 10 | Test allocator over mimalloc | Keep. Miri cannot run mimalloc. |
| `src/backend/linux/native.rs` | 10 | `pre_exec`, sigmask ptrace, tgkill, pidfd, siginfo unions, waitpid, ptrace with a signal | pidfd and waitpid to rustix (step 2), `pre_exec` shared (step 3), siginfo from bytes (step 6), raw ptrace moved (step 3). |
| `src/backend/linux/native_tracee.rs` | 9 | `pre_exec`, waitid, `perf_event_open` | rustix waitid (step 2), shared `pre_exec` (step 3), `perf::sys::open` (step 2). |
| `sdk/rust/src/lib.rs` | 5 | WASM host imports | Keep. |
| `src/main.rs` | 4 | `remove_var` for the flight recorder, global allocator | `env_remove` at spawn (step 4). |
| `src/cli/config.rs` | 3 | `remove_var` for `USCOPE_CONFIG` | `env_remove` at spawn (step 4). |
| `src/cli/syscalls.rs` | 2 | `localtime_r` | Keep. The alternatives are worse. |
| `tests/support/mod.rs` | 2 | tgkill | Keep. Neither nix nor rustix wraps it. |
| `src/backend/linux/syscalls/ptrace.rs` | 2 | `PTRACE_GETSIGINFO` as bytes, SIGURG handler | Move the request (step 3). Keep the handler (section 5). |
| `src/backend/linux/syscalls/launch.rs` | 1 | `pre_exec` | Shared `pre_exec` (step 3). |
| `src/backend/linux/syscalls/info.rs` | 1 | `PTRACE_GET_SYSCALL_INFO` as bytes | Move the request (step 3). |
| `src/backend/linux/perf/mod.rs` | 1 | `pre_exec` | Shared `pre_exec` (step 3). |

Afterwards, the unsafe code in uscope itself lives in:

- `linux/sys.rs` (new): the one `pre_exec`, tgkill, and the raw ptrace
  requests nix cannot make (real-time signals, signal masks, siginfo and
  syscall info as bytes).
- `linux/tls/thread_db.rs`: libthread_db.
- `linux/perf/sys.rs`: `perf_event_open`, its ioctls, the ring mapping, rdtsc.
- `timings/alloc.rs`: the counting allocator and mimalloc's option.
- `cli/syscalls.rs`: `localtime_r`.
- `syscalls/ptrace.rs`: the SIGURG handler.

Plus the probe SDK, the WASM view SDK, and test support.

## 3. Miri

- Add `"miri"` to the toolchain's extensions in `flake.nix`. The nightly
  is already pinned.
- `just miri` runs an allowlist, never the suite: most tests launch
  processes, and Miri runs 50–100× slower. The recipe names the uscope
  test paths it runs, then the probe SDK's tests with `enabled`.
- Concurrency tests get `-Zmiri-many-seeds`. Others run with one seed.
- Miri's sysroot builds under `target/`, inside the hermetic shell, not
  in the home directory.
- Measure the cold and warm cost of each half. The SDK should take
  seconds. If the uscope half takes minutes, `just miri` stays out of
  `just` and runs before merging changes to the files above, as
  `just stress` does.
- Miri tests are small: one unsafe operation each (a wrap, a retire and
  reuse, a callback), with few iterations.

## 4. Steps

Each step lists how we know it worked.

- [x] **1. Miri setup.** Flake extension, `just miri`, running the
  existing `timings::tests::allocator` tests. *Done when* `just miri`
  passes, and both its cold and warm times are recorded in section 6.
- [x] **2. Safe wrappers.** Add rustix with `process`, `param`, `event`
  as needed. Replace `pidfd_open`, `pidfd_send_signal`, `waitpid` (raw
  status via `WaitStatus::as_raw`, so real-time signals still work),
  `waitid` with `__WALL` (`WaitIdOptions::from_bits_retain`), `read`,
  `poll`, `sysconf`. `native_tracee` opens perf events through
  `perf::sys::open`. *Done when* the focused run-control, signal, and
  profiling tests pass, and the `wait_for` path still reports real-time
  signal stops (an existing test, or a new one first).
- [x] **3. One `linux/sys.rs`.** One `pre_exec` helper, taking the
  checks each caller needs (death signal, parent check, personality,
  TRACEME, the gate pipe, the seccomp filter), with safe rustix calls
  inside. The raw ptrace requests and tgkill move here. *Done when*
  launch, attach, profiling launch, and syscall-trace launch tests pass,
  and `just stress` passes.
- [ ] **4. Environment.** First a test: the debuggee sees neither
  `USCOPE_CONFIG` nor `USCOPE_FLIGHT_RECORDING`, under each way of
  launching (debugger, profiler, syscall tracer). Then remove the
  `remove_var` calls, and `env_remove` at each spawn. *Done when* that
  test passes, and the flight recorder's stack-layout reason still holds
  (the debuggee's environment matches a release build's).
- [ ] **5. `Ring::drain` under Miri.** Let a ring be built over a heap
  buffer as well as a mapping. Miri tests act as the kernel: records that
  wrap, a full ring, an empty one. *Done when* `just miri` covers them, and
  the profiler benchmark shows no change.
- [ ] **6. siginfo from bytes.** `signal_metadata` decodes the sender
  and fault address from the 128 bytes `PTRACE_GETSIGINFO` returns, as
  `syscalls/ptrace.rs` already does, with no union reads. *Done when* the
  signal and crash tests pass, including a fault address and a sender pid.
- [ ] **7. Probe SDK.** Each thread keeps a `&'static` ring; reuse goes
  through a safe in-process registry beside the ABI's `AtomicPtr` one;
  records are written with relaxed atomic stores. Unsafe remains for
  allocating a header with its records and for walking the sites section.
  `cfg(miri)` stubs stand in for the section bounds, `pthread_atfork`, and
  `atexit`. Miri tests: threads that emit, exit, and reuse rings, with
  many seeds. *Done when* the probe recording tests pass, `just
  probe-cost` shows no regression, and `just miri` covers the SDK.
- [ ] **8. libthread_db callbacks under Miri.** Call `ps_pdread`,
  `ps_pglobal_lookup`, and `ps_getpid` directly, with a boxed handle and a
  fake `ProcessServices`. Check that no `&mut` path to the boxed handle
  is live while a callback runs. *Done when* `just miri` covers them.
- [ ] **9. Keep it there.** A lint step that fails when
  `allow(unsafe_code)` appears outside the files listed in section 2.
  `dev/` says where unsafe code may live and that `just miri` covers it.
  *Done when* `just` passes and the check fails on a planted site.

## 5. Not doing

- **The SIGURG handler.** It breaks a blocking `waitpid`. Replacing it
  changes how that thread waits; worth its own plan if ever.
- **`localtime_r`.** Parsing TZif ourselves or printing UTC would replace
  one well-understood call with more risk.
- **The mimalloc option.** One line; any other route is unsafe too.
- **Fuzz corpora under Miri.** Slow, and the parsers' crates run Miri
  upstream.
- **Debuggee fixtures.** Their unsafe code is what they test.

## 6. Log

Decisions, measurements, and surprises, newest last.

- 2026-10-13: Plan written. `_rdtsc` is still unsafe on the pinned
  nightly. rustix 1.1.5 is already a transitive dependency.
- 2026-10-13, step 1: `just miri` takes 33 s cold and 15 s warm. Nearly
  all of it is Miri's runner checking the whole crate again before it
  runs; a filter that matches no test takes as long. It joins `just all`,
  not `just`.
- Under Miri the test process keeps Rust's default allocator. Installing
  the capped, counting allocator over `System` instead hid every layout
  check: Miri checks a freed block's layout only on its own allocator,
  and `System` frees through libc's `free`, which takes no size. The
  allocator tests' blocks come from Rust's allocator under Miri, so a
  wrong layout in `Counting` is caught (checked by mutation).
- Miri's weak-memory emulation broke the allocator test's exact process
  totals: the harness's main thread allocates as the test starts, and its
  relaxed counts reached the test late, a block count stale and a byte
  count fresh. Natively the same race was only unlikely. The test now
  reads its own threads' slots exactly (`thread_counts`, test-only) and
  the process's totals as bounds, and passes 32 seeds.
- Code under `cfg(miri)` gets rustc's lints when Miri builds it, but not
  Clippy's.
- 2026-10-13: `just all` found three flakes, each fixed with a test that
  fails without the fix:
  - The allocator totals test (above): it now starts a thread that
    allocates inside its window, which fails the old exact totals natively
    and under Miri.
  - `task_snapshots_find_each_goroutine_where_it_waits_or_runs` expected
    every `main.spin` sample to name the spinner, but a sample of Go's
    preemption signal handler on its thread names no task by design. The
    assertion moved to `spinner_of`, tested on the samples the failure
    recorded.
  - The tokio steps invariant stopped a thread in `memcpy@plt`, which mold
    describes with no CFI, so its backtrace ended there. Stubs without CFI
    now get frames decoded at load (`elf::plt_frames`, the image's
    `StubFrames` table), tested on mold-linked fixtures and a new
    `disassembly-gcc-mold` build. It costs a mold-linked image a few
    allocations per load, so `bench/baseline.json` needs recording again.
- `bench-smoke` cannot pass in a worktree at another path: fixtures embed
  their checkout's path, so their digests differ from the baseline's.
- 2026-10-13, step 2: rustix's `waitpid(None, ..)` is `wait4(0, ..)`, the
  caller's process group, not every child; `wait` is the -1. The waiter
  takes every status through -1, so the wrong one would hang on any
  tracee outside the debugger's group, and no test covered that: a new
  attach test of a process leading its own group times out with it.
  rustix runs `waitid`'s NOWAIT probe too, and `native_tracee` opens its
  breakpoint events through `perf::sys::open`. 16 unsafe sites fewer.
- `just sim 600` then failed liveness on a seed under PCT at depth 1. The
  first session's controller never ran out of work: its preempted calls
  kept reaping the stops of a thread spinning on `sched_yield`, so the
  follower, the highest-priority actor's only rival, never handed a held
  child to a session, and its parent waited for it forever. A simulator
  bug, not the debugger's: PCT now runs an actor that has waited 10,000
  steps, tested first in `schedule.rs`.
- 2026-10-13, step 3: `sys::before_exec` is the one `pre_exec`. It takes
  what each launch needs, in a fixed order: the death signal and parent
  check, fixed addresses, the gate, the seccomp filter, then TRACEME.
  Inside it, only adopting the gate's inherited descriptors and the
  seccomp call are unsafe; the rest is rustix and nix. Mutating it showed
  that no test noticed a child that kept its copy of the gate, or one
  without the death signal. Two `launch` tests now fail on each. The raw
  ptrace requests and tgkill moved there too. Outside `sys.rs`, the
  backend's unsafe code is now `signal_metadata` (step 6) and the SIGURG
  handler.
- `just sim 600` then found a stepping bug that predates this work. A
  step over that returned into the middle of its caller's line went on by
  single steps, and reached the start of the next line, where a
  breakpoint declined its hit. It deferred judging the site to the trap
  there, but a signal arrived first. When the handler returned to the
  site, the step stepped over the trap without judging it, and stopped an
  instruction late. It now judges the site there. A clang build of
  `signal-steps` reproduces it natively: its caller signals itself as the
  last instruction of the line it returns into.
- `just stress` then turned up six flakes, none caused by this work, each
  now pinned by a test that failed first:
  - A capture rebuilt from chunks lost its tasks' labels, and inverted
    zones of equal span, which TSC conversion and clamping can make.
    The browser rebuilds a profile when it symbolizes a vDSO sample, so
    both showed up in the web profile tests.
  - The ptrace syscall engine could see a new thread's first stop, or
    its exit, before the clone or fork event that made it, and announced
    a thread as a process, or ended a child it had not announced. Such
    tracees are now held until their event arrives.
  - The probe collector counted a ring as malformed when the program
    exited between reading its registry and its rings. A ring that
    cannot be read because the process has gone is not malformed.
  - A profiler oracle rejected a sample taken in the worker itself.
  - The strace comparison replaced pipe inodes with `N` but kept the
    padding strace computed from their real width, so two runs whose
    inodes had different digit counts differed by a space.
  - A recording-window test read `uscope report`'s top 20 functions,
    which unsymbolized vDSO addresses, a row each, can fill, leaving the
    phase it looked for out. It now reads the whole tree. Of 40
    recordings, the top 20 left the phase out of 21.
- `just sim 600` then found an attach bug that predates this work. The
  thread listing saw only the leader, which then created a thread and
  exited alone, too soon to be seized. With nothing seized, attach said
  the inferior had not been launched, though the process lived on in the
  new thread. While nothing is seized, attach now lists again until it
  seizes a thread or finds none it has not tried. A controller test
  pins it.
