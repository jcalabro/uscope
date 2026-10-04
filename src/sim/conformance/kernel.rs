//! The simulated kernel's rules, each checked against Linux.
//!
//! Every test runs one script of ptrace operations on each variant of a
//! golden program, natively and simulated, and requires the same
//! observations (see `tracee`). A rule the simulation models without such a
//! test is a guess.

use super::tracee::dual_run;
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
