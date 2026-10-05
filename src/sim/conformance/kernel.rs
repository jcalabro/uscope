//! The simulated kernel's rules, each checked against Linux.
//!
//! Every test runs one script of ptrace operations on each variant of a
//! golden program, natively and simulated, and requires the same
//! observations (see `tracee`). A rule the simulation models without such a
//! test is a guess.

use nix::libc;

use super::tracee::{dual_run, dual_run_then_exit};
use crate::sim::cpu::{RAX, RDI, RSP};

/// K-EXEC-1: a launched program first stops at its entry point for a
/// SIGTRAP it sent itself, at the end of `execve`, and is named after its
/// executable.
#[test]
fn k_exec_1_a_launched_program_stops_at_its_entry() {
    dual_run("straight", &[], |record| {
        let leader = record.leader();
        record.stop(leader);
        record.system_call(leader);
        let name = record.tracee.name(leader);
        record.note(format!("name: {name}"));
    });
}

/// K-EXEC-2: a program loads where its image says, or, position-independent
/// and without an interpreter, as the first mapping below the mmap base,
/// with randomization off. Its maps name its file at each segment.
#[test]
fn k_exec_2_a_program_loads_where_linux_puts_it() {
    dual_run("frames", &[], |record| {
        let leader = record.leader();
        let (registers, _) = record
            .tracee
            .registers(leader)
            .expect("registers at the first stop");
        record.note(format!("rip: {:#x}", registers.rip));
        let maps = record.tracee.maps(leader).unwrap_or_default();
        for line in maps.lines() {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.last().is_some_and(|path| path.contains("/frames-")) {
                record.note(format!("mapped: {}", fields[..3].join(" ")));
            }
        }
    });
}

/// K-TRAP-1: `int3` reports SIGTRAP with `SI_KERNEL` and `rip` past the
/// trap, outside any system call; a single step reports `TRAP_TRACE` at
/// the next instruction; a single step across `syscall` reports
/// `TRAP_BRKPT` at the call's exit.
#[test]
fn k_trap_1_traps_report_their_kind_and_place() {
    dual_run("straight", &[], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let fib = record.landmarks.symbol("fib");
        let original = record.plant(fib);
        record.resume(leader, None);
        record.wait(leader);
        record.stop(leader);
        record.system_call(leader);
        record.restore(leader, fib, original);
        record.rewind(leader);
        record.step(leader);
        record.wait(leader);
        record.stop(leader);

        let syscall = record.landmarks.syscall;
        let original = record.plant(syscall);
        record.resume(leader, None);
        record.wait(leader);
        record.restore(leader, syscall, original);
        record.rewind(leader);
        record.step(leader);
        record.wait(leader);
        record.stop(leader);
        record.system_call(leader);
    });
}

/// K-EXIT-1, single-threaded: `exit_group` stops at the exit event inside
/// the call, with the status in its message and `si_code` `0x605`;
/// continuing reports the exit. K-WAIT-1: requests on a thread that is not
/// stopped fail with ESRCH.
#[test]
fn k_exit_1_an_exiting_thread_stops_at_its_exit_event() {
    dual_run("straight", &["1"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        record.wait(leader);
        record.stop(leader);
        record.system_call(leader);
        record.event_message(leader);
        record.resume(leader, None);
        record.wait(leader);
        let result = record.tracee.registers(leader).map(drop);
        record.result("registers of a reaped thread", result);
    });
}

/// K-EXIT-3: SIGKILL takes a thread out of a signal-delivery-stop to its
/// exit event, with message 9, and then reports it killed. K-EXIT-4: a
/// thread already at its exit event stays there through another SIGKILL.
#[test]
fn k_exit_3_sigkill_ends_a_stopped_thread_through_its_exit_event() {
    dual_run("straight", &[], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let fib = record.landmarks.symbol("fib");
        record.plant(fib);
        record.resume(leader, None);
        record.wait(leader);
        let result = record.tracee.kill();
        record.result("kill", result);
        record.wait(leader);
        record.stop(leader);
        record.event_message(leader);
        let result = record.tracee.kill();
        record.result("kill again", result);
        let result = record.tracee.registers(leader).map(drop);
        record.result("registers at the exit event", result);
        record.resume(leader, None);
        record.wait(leader);
    });
}

/// K-EXIT-4: a thread at its exit event from `exit_group` stays there
/// through SIGKILL and then reports the exit it was making.
#[test]
fn k_exit_4_sigkill_leaves_a_thread_at_its_exit_event() {
    dual_run("straight", &["0"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        record.wait(leader);
        let result = record.tracee.kill();
        record.result("kill", result);
        record.stop(leader);
        record.resume(leader, None);
        record.wait(leader);
    });
}

/// K-EXIT-4: a thread at the exit event of its own `exit` is taken out of
/// it when its group starts exiting, by `exit_group` or SIGKILL alike: it
/// finishes exiting and reports the group's status.
#[test]
fn k_exit_4_a_group_exit_ends_a_thread_at_its_own_exit_event() {
    for kill in [false, true] {
        dual_run("threads", &["2", "worker"], |record| {
            let leader = record.leader();
            record.set_options(leader);
            let share = record.landmarks.symbol("share");
            let original = record.plant(share);
            let mut workers = Vec::new();
            for _ in 0..2 {
                record.resume(leader, None);
                let worker = record.cloned(leader);
                record.wait(worker);
                workers.push(worker);
            }
            record.resume(leader, None);
            for &worker in &workers {
                record.resume(worker, None);
            }
            for &worker in &workers {
                record.wait(worker);
            }
            record.restore(workers[0], share, original);
            for &worker in &workers {
                record.rewind(worker);
            }
            // The first worker finishes first and waits at its exit event.
            record.resume(workers[0], None);
            record.wait(workers[0]);
            record.event_message(workers[0]);
            if kill {
                let result = record.tracee.kill();
                record.result("kill", result);
            } else {
                // The second exits the group.
                record.resume(workers[1], None);
            }
            record.wait(workers[1]);
            record.event_message(workers[1]);
            let result = record.tracee.registers(workers[0]).map(drop);
            record.result("registers of the first worker", result);
            let result = record.tracee.resume(workers[0], None, false);
            record.result("continue the first worker", result);
            record.wait(workers[0]);
            record.wait(leader);
            record.event_message(leader);
            record.resume(workers[1], None);
            record.wait(workers[1]);
            record.resume(leader, None);
            record.wait(leader);
        });
    }
}

/// K-SIG-1: SIGSTOP from the tracer's `tgkill` to a stopped thread waits
/// until the thread resumes, then stops it with `SI_TKILL` from the
/// tracer before it runs. Continuing without the signal suppresses it.
#[test]
fn k_sig_1_a_tracer_stop_request_stops_the_thread() {
    dual_run("straight", &[], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let result = record.tracee.request_stop(leader);
        record.result("tgkill SIGSTOP", result);
        record.resume(leader, None);
        record.wait(leader);
        record.stop(leader);
        record.resume(leader, None);
        record.wait(leader);
        record.stop(leader);
    });
}

/// K-SIG-1, for a running thread: the tracer's SIGSTOP stops it with
/// `SI_TKILL` from the tracer. Where it stops depends on timing, so only
/// the signal is compared.
#[test]
fn k_sig_1_a_tracer_stop_request_stops_a_running_thread() {
    dual_run("racing-exit", &["2", "4"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        // Worker 0 stays at its first stop, so nothing exits the group.
        let first = record.cloned(leader);
        record.resume(leader, None);
        let second = record.cloned(leader);
        record.resume(leader, None);
        record.wait(first);
        record.wait(second);
        record.resume(second, None);
        let result = record.tracee.request_stop(second);
        record.result("tgkill SIGSTOP", result);
        record.wait(second);
        let signal = record
            .tracee
            .signal(second)
            .map(|(signal, code, sender, _)| (signal, code, sender.map(|tid| record.name_of(tid))));
        record.note(format!("siginfo of thread 2: {signal:?}"));
    });
}

/// K-MEM-1: ptrace writes ignore page protections and fail with EIO where
/// nothing is mapped, as reads do.
#[test]
fn k_mem_1_ptrace_ignores_protections_but_not_holes() {
    dual_run("straight", &[], |record| {
        let leader = record.leader();
        let entry = record.landmarks.entry;
        let word = record
            .tracee
            .peek(leader, entry)
            .expect("read the entry point");
        let result = record.tracee.poke(leader, entry, word ^ 0xff);
        record.result("poke code", result);
        let changed = record
            .tracee
            .peek(leader, entry)
            .map(|changed| changed ^ word);
        record.note(format!("changed: {changed:?}"));
        let unmapped = record.landmarks.unmapped;
        let result = record.tracee.peek(leader, unmapped).map(drop);
        record.result("peek unmapped", result);
        let result = record.tracee.poke(leader, unmapped, 0);
        record.result("poke unmapped", result);
    });
}

/// K-CLONE-1: a creator tracing clones stops at `PTRACE_EVENT_CLONE` inside
/// the call, whose message names the new thread in its own group; the new
/// thread, on its own stack and returning zero from the call, first stops
/// for a SIGSTOP nobody sent. Continuing the creator returns the new id.
/// A thread ending alone stops at its exit event inside `exit`, and the
/// last thread's `exit_group` reports the program's status.
#[test]
fn k_clone_1_a_new_thread_starts_stopped() {
    dual_run("threads", &["1", "main"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        let child = record.cloned(leader);
        record.wait(child);
        record.stop(child);
        record.system_call(child);
        let stack = record
            .tracee
            .registers(child)
            .map(|(registers, _)| registers.general[RSP] - record.landmarks.symbol("stacks"));
        record.note(format!("stack of thread 1, from stacks: {stack:x?}"));
        let name = record.tracee.name(child);
        record.note(format!("name of thread 1: {name}"));
        record.step(leader);
        record.wait(leader);
        record.stop(leader);
        let returned = record
            .tracee
            .registers(leader)
            .map(|(registers, _)| registers.general[RAX] == u64::from(child.cast_unsigned()));
        record.note(format!("clone returned thread 1: {returned:?}"));
        // The leader stays stopped until the worker is gone, or its group
        // exit would race the worker's own.
        record.resume(child, None);
        record.wait(child);
        record.stop(child);
        record.system_call(child);
        record.event_message(child);
        record.resume(child, None);
        record.wait(child);
        let group = record.tracee.thread_group(child);
        record.note(format!("thread 1 is gone: {}", group.is_none()));
        record.resume(leader, None);
        record.wait(leader);
        record.system_call(leader);
        record.event_message(leader);
        record.resume(leader, None);
        record.wait(leader);
    });
}

/// K-EXIT-1 and K-EXIT-2, with siblings: `exit_group` takes running
/// siblings, siblings held in signal-delivery-stops, and a creator held at
/// its clone event to their exit events with the group's status. K-WAIT-2:
/// the leader's exit is reported only once every other thread is reaped.
#[test]
fn k_exit_2_exit_group_ends_every_sibling_through_its_exit_event() {
    dual_run("racing-exit", &["3", "2"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        let first = record.cloned(leader);
        record.resume(leader, None);
        let second = record.cloned(leader);
        record.resume(leader, None);
        let third = record.cloned(leader);
        // The leader stays at its third clone event, and the second worker
        // at its first stop, while the first worker runs to its exit_group
        // beside the third.
        record.wait(first);
        record.wait(second);
        record.wait(third);
        record.resume(third, None);
        record.resume(first, None);
        record.wait(first);
        record.stop(first);
        record.system_call(first);
        record.event_message(first);
        for thread in [leader, second] {
            record.wait(thread);
            record.stop(thread);
            record.event_message(thread);
        }
        // Where the running third stopped depends on timing.
        record.wait(third);
        record.event_message(third);
        // The leader returned from its clone call on the way out.
        let leader_call = record
            .tracee
            .registers(leader)
            .map(|(registers, orig_rax)| {
                (
                    registers.general[RAX] == u64::from(third.cast_unsigned()),
                    orig_rax,
                )
            });
        record.note(format!(
            "leader returned thread 3, orig_rax: {leader_call:?}"
        ));
        record.resume(leader, None);
        record.has_report(leader);
        for thread in [first, second, third] {
            record.resume(thread, None);
            record.wait(thread);
        }
        record.wait(leader);
    });
}

/// K-EXIT-3, with siblings: SIGKILL ends every thread through its exit
/// event with message 9, held ones included, and the leader is reported
/// killed last.
#[test]
fn k_exit_3_sigkill_ends_every_thread_through_its_exit_event() {
    dual_run("threads", &["3", "main"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let mut workers = Vec::new();
        for _ in 0..3 {
            record.resume(leader, None);
            let worker = record.cloned(leader);
            record.wait(worker);
            workers.push(worker);
        }
        let result = record.tracee.kill();
        record.result("kill", result);
        for &thread in std::iter::once(&leader).chain(&workers) {
            record.wait(thread);
            record.event_message(thread);
        }
        record.resume(leader, None);
        record.has_report(leader);
        for &worker in &workers {
            record.resume(worker, None);
            record.wait(worker);
        }
        record.wait(leader);
    });
}

/// K-EXIT-5: a leader that exits alone stops at its exit event, then stays
/// a zombie: ptrace requests fail, `tgkill` still succeeds, its maps read
/// empty, and its exit is not reported while another thread lives.
/// K-EXIT-6: the process then ends with the status of the thread that
/// exited last.
#[test]
fn k_exit_5_a_leader_exiting_alone_waits_for_its_threads() {
    dual_run("threads", &["1", "leader"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        let worker = record.cloned(leader);
        record.wait(worker);
        record.resume(leader, None);
        record.wait(leader);
        record.stop(leader);
        record.system_call(leader);
        record.event_message(leader);
        record.resume(leader, None);
        // The leader's exit completes on its own; wait until its maps are
        // gone, which shows it is a zombie.
        let started = std::time::Instant::now();
        while record
            .tracee
            .maps(leader)
            .is_some_and(|maps| !maps.is_empty())
        {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "the leader never finished exiting"
            );
            std::thread::yield_now();
        }
        record.note("the leader's maps read empty");
        let result = record.tracee.registers(leader).map(drop);
        record.result("registers of the zombie leader", result);
        let result = record.tracee.request_stop(leader);
        record.result("tgkill SIGSTOP to the zombie leader", result);
        let group = record.tracee.thread_group(leader);
        record.note(format!("the zombie leader is listed: {}", group.is_some()));
        record.has_report(leader);
        record.resume(worker, None);
        record.wait(worker);
        record.event_message(worker);
        record.resume(worker, None);
        record.wait(worker);
        record.wait(leader);
    });
}

/// K-EXIT-5: an untraced leader that exited alone cannot be seized while it
/// is a zombie, any more than a thread the tracer already traces.
#[test]
fn k_exit_5_a_zombie_cannot_be_seized() {
    dual_run("threads", &["1", "leader"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        let worker = record.cloned(leader);
        record.wait(worker);
        record.detach(leader, None);
        let started = std::time::Instant::now();
        while !record.tracee.is_zombie(leader) {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "the leader never finished exiting"
            );
            record.tracee.pass_time();
        }
        record.seize(leader);
        record.seize(worker);
        record.resume(worker, None);
        record.wait(worker);
        record.event_message(worker);
        record.resume(worker, None);
        record.wait(worker);
    });
}

/// K-EXIT-6: the thread that begins to exit last, before its exit event,
/// starts a group exit with its status, which every thread reaped later
/// reports. Holding the leader at its exit event changes nothing; holding it
/// at its clone event until the worker is gone makes its own status the
/// process's. Every thread of the program exits with 3, so the worker's
/// status is changed to 5 as it calls `rt_exit`.
#[test]
fn k_exit_6_the_last_thread_to_begin_exiting_decides_the_status() {
    for leader_last in [false, true] {
        dual_run("threads", &["1", "leader"], |record| {
            let leader = record.leader();
            record.set_options(leader);
            record.resume(leader, None);
            let worker = record.cloned(leader);
            record.wait(worker);
            if !leader_last {
                record.resume(leader, None);
                record.wait(leader);
                record.event_message(leader);
            }
            let rt_exit = record.landmarks.symbol("rt_exit");
            let original = record.plant(rt_exit);
            record.resume(worker, None);
            record.wait(worker);
            record.restore(worker, rt_exit, original);
            record.rewind(worker);
            let (mut registers, _) = record.tracee.registers(worker).expect("registers");
            registers.general[RDI] = 5;
            let result = record.tracee.set_registers(worker, &registers);
            record.result("exit with 5", result);
            record.resume(worker, None);
            record.wait(worker);
            record.event_message(worker);
            record.resume(worker, None);
            record.wait(worker);
            if leader_last {
                record.resume(leader, None);
                record.wait(leader);
                record.event_message(leader);
            }
            record.resume(leader, None);
            record.wait(leader);
        });
    }
}

/// K-EXIT-6: a thread that exited alone but is not yet reaped reports the
/// group's status once the group exits.
#[test]
fn k_exit_6_a_group_exit_decides_the_status_of_unreaped_threads() {
    dual_run("threads", &["2", "worker"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let share = record.landmarks.symbol("share");
        let original = record.plant(share);
        let mut workers = Vec::new();
        for _ in 0..2 {
            record.resume(leader, None);
            let worker = record.cloned(leader);
            record.wait(worker);
            workers.push(worker);
        }
        record.resume(leader, None);
        // Both workers pass the barrier and stop at their first share.
        for &worker in &workers {
            record.resume(worker, None);
        }
        for &worker in &workers {
            record.wait(worker);
        }
        record.restore(workers[0], share, original);
        for &worker in &workers {
            record.rewind(worker);
        }
        // The first worker finishes first, so it exits alone.
        record.resume(workers[0], None);
        record.wait(workers[0]);
        record.event_message(workers[0]);
        record.resume(workers[0], None);
        // The second exits the group while the first is a zombie.
        record.resume(workers[1], None);
        record.wait(workers[1]);
        record.event_message(workers[1]);
        record.wait(leader);
        record.event_message(leader);
        record.wait(workers[0]);
        record.resume(workers[1], None);
        record.wait(workers[1]);
        record.resume(leader, None);
        record.wait(leader);
    });
}

/// DR7 for slot 0 watching eight bytes for stores.
const WRITE_8: u64 = 1 | 1 << 16 | 2 << 18;
/// DR6 with no condition recorded.
const DR6_IDLE: u64 = 0xffff_0ff0;

/// Whether a DR6 value records a slot's hit.
const fn records_hit(dr6: u64) -> bool {
    dr6 & 0xf != 0
}

/// K-DR-1: a store a slot watches raises SIGTRAP with `TRAP_HWBKPT` and
/// `rip` after the instruction, and DR6 holds the slot's bit. DR6 does not
/// change at a stop that is no debug exception, such as a breakpoint's.
#[test]
fn k_dr_1_a_watched_store_traps_after_the_instruction() {
    dual_run("stores", &["2", "0"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let counter = record.landmarks.symbol("counter");
        let _ = record.debug(leader, 6);
        record.watch(leader, counter, WRITE_8);
        let _ = record.debug(leader, 0);
        let _ = record.debug(leader, 7);
        record.resume(leader, None);
        record.wait(leader);
        record.stop(leader);
        record.rip(leader);
        let _ = record.debug(leader, 6);
        let bump = record.landmarks.symbol("bump");
        record.plant(bump);
        record.resume(leader, None);
        record.wait(leader);
        record.stop(leader);
        let _ = record.debug(leader, 6);
    });
}

/// K-DR-2: single-stepping over a watched store gives one stop,
/// `TRAP_TRACE`, with DR6 holding both the step's bit and the slot's.
#[test]
fn k_dr_2_a_step_over_a_watched_store_reports_both() {
    dual_run("stores", &["2", "0"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let counter = record.landmarks.symbol("counter");
        record.watch(leader, counter, WRITE_8);
        record.resume(leader, None);
        record.wait(leader);
        record.set_debug(leader, 6, DR6_IDLE);
        // Step to the next round's store of the counter.
        let mut steps = 0;
        loop {
            record.tracee.resume(leader, None, true).expect("step");
            let status = record.tracee.wait(leader);
            steps += 1;
            let dr6 = record.tracee.read_debug(leader, 6).expect("read DR6");
            if records_hit(dr6) || steps == 10_000 {
                let status = status
                    .to_string()
                    .replacen(&leader.to_string(), "leader", 1);
                record.note(format!("steps to the store: {steps}, last {status}"));
                break;
            }
        }
        record.stop(leader);
        record.rip(leader);
        let _ = record.debug(leader, 6);
    });
}

/// K-DR-3: a new thread starts with no slot armed, so it stores to the
/// watched counter without trapping, though its DR7 reads as its
/// creator's.
#[test]
fn k_dr_3_a_new_thread_starts_unarmed() {
    dual_run("stores", &["1", "1"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let shared = record.landmarks.symbol("shared");
        record.watch(leader, shared, WRITE_8);
        record.resume(leader, None);
        let worker = record.cloned(leader);
        record.wait(worker);
        record.stop(worker);
        for index in [0, 6, 7] {
            let _ = record.debug(worker, index);
        }
        // The worker adds to the counter and exits, without a trap.
        record.resume(worker, None);
        record.wait(worker);
        record.event_message(worker);
    });
}

/// K-DR-4: writing an address reserves a hardware breakpoint even while
/// disabled, which fails with `ENOSPC` once others hold every slot. A DR7
/// write a slot refuses, as for an address its length misaligns, changes
/// nothing. No slot may watch the top page of user memory.
#[test]
fn k_dr_4_slots_are_reserved_and_dr7_is_transactional() {
    dual_run("stores", &["1", "0"], |record| {
        let leader = record.leader();
        let counter = record.landmarks.symbol("counter");
        let steady = record.landmarks.symbol("steady");
        record.tracee.hold_debug_slots(leader, 3, counter);
        record.set_debug(leader, 0, counter);
        record.set_debug(leader, 1, steady);
        record.set_debug(leader, 0, counter + 4);
        record.set_debug(leader, 7, WRITE_8);
        let _ = record.debug(leader, 7);
        let _ = record.debug(leader, 0);
        record.set_debug(leader, 0, 0x7fff_ffff_f000);
        record.set_debug(leader, 5, 0);
    });
}

/// K-DR-5: a store of the value already there traps, a ptrace write does
/// not, and `rep stos` traps once per iteration that touches the watched
/// bytes, with `rip` still at the instruction.
#[test]
fn k_dr_5_every_store_traps_but_ptrace_writes() {
    dual_run("stores", &["1", "0"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let steady = record.landmarks.symbol("steady");
        record.watch(leader, steady, WRITE_8);
        let value = record.tracee.peek(leader, steady).expect("read steady");
        let result = record.tracee.poke(leader, steady, value);
        record.result("poke steady", result);
        record.resume(leader, None);
        record.wait(leader);
        record.stop(leader);
        record.rip(leader);
        let pattern = record.landmarks.symbol("pattern");
        record.watch(leader, pattern, WRITE_8);
        for _ in 0..3 {
            record.resume(leader, None);
            record.wait(leader);
            record.stop(leader);
            record.rip(leader);
        }
    });
}

/// K-FORK-1: a fork stops the parent at `PTRACE_EVENT_FORK` inside the
/// call, naming the child, which leads its own group and is listed as the
/// forking thread's child. The child starts stopped, traced with the
/// parent's options, in a copy of its memory, traps included, with no
/// debug register armed though DR7 reads as the parent's (K-DR-3).
#[test]
fn k_fork_1_a_fork_child_starts_stopped_in_a_copy_of_its_parent() {
    dual_run("fork", &["1", "0"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let child_code = record.landmarks.symbol("child");
        let original = record.plant(child_code);
        let unmapped = record.landmarks.unmapped;
        record.watch(leader, unmapped, WRITE_8);
        record.resume(leader, None);
        let child = record.forked(leader);
        record.wait(child);
        record.stop(child);
        record.system_call(child);
        let _ = record.debug(child, 0);
        let _ = record.debug(child, 7);
        let byte = record
            .tracee
            .peek(child, child_code)
            .map(|word| word & 0xff);
        record.note(format!("the child's byte at child: {byte:x?}"));

        // The child traps where its parent's memory did, and reports its
        // own exit, as its parent's options say.
        record.resume(child, None);
        record.wait(child);
        record.stop(child);
        record.restore(child, child_code, original);
        record.rewind(child);
        let byte = record
            .tracee
            .peek(leader, child_code)
            .map(|word| word & 0xff);
        record.note(format!("the parent's byte at child: {byte:x?}"));
        record.resume(child, None);
        record.wait(child);
        record.event_message(child);
        record.resume(child, None);
        record.wait(child);

        // Once the tracer reaped it, the child is its parent's to reap,
        // with SIGCHLD.
        record.restore(leader, child_code, original);
        record.set_debug(leader, 7, 0);
        record.resume(leader, None);
        record.wait(leader);
        record.signal(leader);
        record.resume(leader, Some(libc::SIGCHLD));
        record.wait(leader);
        record.event_message(leader);
        record.children_of(leader);
        record.resume(leader, None);
        record.wait(leader);
    });
}

/// K-FORK-2: a detached child runs untraced. Its requests fail with ESRCH,
/// its parent reaps it after SIGCHLD, and a trap it executes kills it with
/// SIGTRAP, which its parent sees. Whether it dumps core depends on the
/// machine, so only the signal is recorded.
#[test]
fn k_fork_2_a_detached_child_runs_untraced() {
    dual_run("fork", &["2", "0"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let child_code = record.landmarks.symbol("child");
        let original = record.plant(child_code);
        record.resume(leader, None);
        let first = record.forked(leader);
        record.wait(first);
        record.restore(first, child_code, original);
        record.detach(first, None);
        let result = record.tracee.registers(first).map(drop);
        record.result("registers of a detached child", result);
        record.resume(leader, None);
        record.wait(leader);
        record.signal(leader);
        record.resume(leader, Some(libc::SIGCHLD));

        // This one keeps the trap it inherited, and dies of it.
        let second = record.forked(leader);
        record.wait(second);
        record.detach(second, None);
        record.restore(leader, child_code, original);
        record.resume(leader, None);
        record.wait(leader);
        record.resume(leader, Some(libc::SIGCHLD));
        record.wait(leader);
        record.event_message(leader);
        record.children_of(leader);
        record.resume(leader, None);
        record.wait(leader);
    });
}

/// K-FORK-3: a child whose parent exits sees its parent change, and its
/// exit is the tracer's to reap.
#[test]
fn k_fork_3_an_orphan_sees_its_parent_change() {
    dual_run("fork", &["1", "1"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        let waited = record.forked(leader);
        record.wait(waited);
        record.detach(waited, None);
        record.resume(leader, None);
        record.wait(leader);
        record.signal(leader);
        record.resume(leader, Some(libc::SIGCHLD));
        let orphan = record.forked(leader);
        record.wait(orphan);
        record.resume(orphan, None);
        record.resume(leader, None);
        record.wait(leader);
        record.event_message(leader);
        record.resume(leader, None);
        record.wait(leader);
        record.wait(orphan);
        record.stop(orphan);
        record.event_message(orphan);
        record.resume(orphan, None);
        record.wait(orphan);
    });
}

/// K-SEIZE-1: a seized thread runs on until an interrupt stops it with
/// `PTRACE_EVENT_STOP`. The threads and processes it creates then are
/// traced with its options, each first stopping in a `PTRACE_EVENT_STOP`
/// of its own rather than for SIGSTOP.
#[test]
fn k_seize_1_a_seized_thread_runs_on_and_its_children_start_in_an_event_stop() {
    dual_run("fork", &["1", "2"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        // The first child stays at its first stop, so its parent waits for
        // it, running untraced once detached.
        let first = record.forked(leader);
        record.wait(first);
        record.detach(leader, None);
        record.seize(leader);
        record.interrupt(leader);
        record.wait(leader);
        record.signal(leader);
        record.event_message(leader);
        record.detach(first, None);
        record.resume(leader, None);
        record.wait(leader);
        record.signal(leader);
        record.resume(leader, Some(libc::SIGCHLD));
        let worker = record.cloned(leader);
        record.wait(worker);
        record.signal(worker);
        record.event_message(worker);
        record.resume(worker, None);
        let second = record.forked(worker);
        record.wait(second);
        record.signal(second);
        record.event_message(second);
    });
}

/// K-INT-1: an interrupt stops a running seized thread with
/// `PTRACE_EVENT_STOP`. One sent while the thread is stopped waits until
/// it resumes, ahead of a pending signal. Where a running thread stops
/// depends on timing, so only signals are compared.
#[test]
fn k_int_1_an_interrupt_stops_a_seized_thread() {
    dual_run("racing-exit", &["2", "4"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        // Worker 0 stays at its first stop, so nothing exits the group.
        let first = record.cloned(leader);
        record.resume(leader, None);
        let second = record.cloned(leader);
        record.resume(leader, None);
        record.wait(first);
        record.wait(second);
        record.detach(second, None);
        record.seize(second);
        record.interrupt(second);
        record.wait(second);
        record.signal(second);

        record.interrupt(second);
        record.resume(second, None);
        let result = record.tracee.request_stop(second);
        record.result("tgkill SIGSTOP", result);
        record.wait(second);
        record.signal(second);
        record.resume(second, None);
        record.wait(second);
        record.signal(second);
    });
}

/// K-INT-2: an interrupt and a stop request succeed for a seized thread at
/// its exit event and for one that is an unreaped zombie, and fail with
/// ESRCH once it is reaped.
#[test]
fn k_int_2_an_interrupt_reaches_an_exiting_thread_until_it_is_reaped() {
    dual_run("racing-exit", &["2", "4"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        let first = record.cloned(leader);
        record.resume(leader, None);
        let second = record.cloned(leader);
        record.wait(first);
        record.wait(second);
        record.detach(second, None);
        record.seize(second);
        // Worker 0 exits the group, which stops every thread at its exit.
        record.resume(first, None);
        record.wait(second);
        record.event_message(second);
        record.interrupt(second);
        let result = record.tracee.request_stop(second);
        record.result("tgkill SIGSTOP at the exit event", result);
        record.resume(second, None);
        while !record.tracee.has_report(second) {
            std::thread::yield_now();
        }
        record.interrupt(second);
        let result = record.tracee.request_stop(second);
        record.result("tgkill SIGSTOP to a zombie", result);
        record.wait(second);
        record.interrupt(second);
        let result = record.tracee.request_stop(second);
        record.result("tgkill SIGSTOP once reaped", result);
    });
}

/// K-WAIT-3: a tracer that exits releases the threads it still traces. A
/// traced leader that exited alone, a zombie no request reaches, joins its
/// process's end as if never traced, and the process's parent reaps the
/// process once its last thread exits. Its tracer traces no clones, so the
/// worker it creates runs untraced, ending whenever it does.
#[test]
fn k_wait_3_a_tracers_exit_releases_a_traced_zombie() {
    dual_run_then_exit("threads", &["1", "leader"], |record| {
        let leader = record.leader();
        record.resume(leader, None);
        let started = std::time::Instant::now();
        while !record.tracee.is_zombie(leader) {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "the leader never finished exiting"
            );
            record.tracee.pass_time();
        }
        record.detach(leader, None);
    });
}

/// K-EXIT-5: an untraced leader that exited alone is a zombie the tracer
/// cannot reach. Once the tracer reaps the last thread, the process ends,
/// and its parent reaps it.
#[test]
fn k_exit_5_reaping_the_last_thread_ends_an_untraced_leaders_process() {
    dual_run_then_exit("threads", &["1", "leader"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        let worker = record.cloned(leader);
        record.wait(worker);
        record.detach(leader, None);
        let started = std::time::Instant::now();
        while !record.tracee.is_zombie(leader) {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "the leader never finished exiting"
            );
            record.tracee.pass_time();
        }
        record.resume(worker, None);
        record.wait(worker);
        record.event_message(worker);
        record.resume(worker, None);
        record.wait(worker);
    });
}

/// K-WAIT-3: a tracer that exits releases a thread on its way out too: a
/// seized one SIGKILL took out of its stop ends untraced, and its parent
/// reaps its process.
#[test]
fn k_wait_3_a_tracers_exit_releases_an_exiting_thread() {
    dual_run_then_exit("fork", &["1", "0"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        let first = record.forked(leader);
        record.wait(first);
        record.detach(leader, None);
        record.seize(leader);
        record.interrupt(leader);
        record.wait(leader);
        record.detach(first, None);
        let result = record.tracee.kill();
        record.result("SIGKILL", result);
    });
}
