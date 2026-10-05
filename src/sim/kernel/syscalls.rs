//! The system calls the golden runtime makes. Any other is a model gap.

use nix::errno::Errno;
use nix::libc;

use super::signals::{SI_USER, SIGSTOP};
use super::{
    ENOSYS_RESULT, ExitStatus, Happening, Kernel, Options, Pending, SigInfo, State, Thread, Tid,
    Tracing,
};
use crate::sim::cpu::{RAX, RDI, RDX, RSI, RSP};

/// `r10`, a system call's fourth argument.
const R10: usize = 10;

const SYS_WRITE: u64 = 1;
const SYS_SCHED_YIELD: u64 = 24;
const SYS_GETPID: u64 = 39;
const SYS_CLONE: u64 = 56;
pub(super) const SYS_FORK: u64 = 57;
pub(super) const SYS_EXECVE: u64 = 59;
const SYS_EXIT: u64 = 60;
const SYS_WAIT4: u64 = 61;
const SYS_GETPPID: u64 = 110;
const SYS_EXIT_GROUP: u64 = 231;

/// The flags of a thread that shares everything with its creator, as the
/// golden runtime creates threads: `CLONE_VM | CLONE_FS | CLONE_FILES |
/// CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM`.
const THREAD_FLAGS: u64 = 0x50f00;

/// Serves the system call a thread just entered with `syscall`, leaving
/// its result in `rax` unless the call stops or ends the thread. Returns
/// whether the thread yielded the CPU.
pub(super) fn serve(kernel: &mut Kernel, tid: Tid) -> bool {
    let registers = kernel.threads[&tid].registers;
    let argument = |index: usize| registers.general[index];
    #[expect(
        clippy::cast_possible_truncation,
        reason = "exit statuses are ints, the low half of the register"
    )]
    let code = argument(RDI) as i32;
    let result = match registers.general[RAX] {
        SYS_WRITE => write(kernel, tid, argument(RDI), argument(RSI), argument(RDX)),
        SYS_SCHED_YIELD => {
            set_result(kernel, tid, 0);
            return true;
        }
        SYS_CLONE => {
            clone(kernel, tid, argument(RDI), argument(RSI));
            return false;
        }
        SYS_FORK => {
            kernel.fork(tid);
            return false;
        }
        SYS_WAIT4 if argument(R10) == 0 => {
            kernel.wait4(tid, argument(RDI), argument(RSI), argument(RDX))
        }
        SYS_GETPID => u64::try_from(kernel.threads[&tid].tgid).expect("process ids are positive"),
        SYS_GETPPID => kernel.parent_of(tid),
        SYS_EXIT => {
            inside_call(kernel, tid);
            let group = kernel.threads[&tid].tgid;
            // K-EXIT-6: the last thread to begin exiting starts a group exit
            // with its status, without disturbing any other thread.
            let last = kernel
                .threads_of(group)
                .all(|thread| thread.tid == tid || thread.state.exiting());
            if last {
                kernel
                    .processes
                    .get_mut(&group)
                    .expect("a thread's process exists")
                    .group_exit
                    .get_or_insert(ExitStatus::Code(code));
            }
            if tid == group {
                kernel.leader_exits.insert(tid, code);
            }
            kernel.reach_exit(tid, ExitStatus::Code(code));
            return false;
        }
        SYS_EXIT_GROUP => {
            inside_call(kernel, tid);
            let group = kernel.threads[&tid].tgid;
            kernel.happenings.push(Happening::GroupExit { tid });
            kernel.kill_process(group, ExitStatus::Code(code));
            // The caller is already in the kernel, and goes straight on to
            // its exit.
            let exit = kernel.processes[&group]
                .group_exit
                .expect("the group is exiting");
            kernel.reach_exit(tid, exit);
            return false;
        }
        number => {
            kernel.gap(format!("system call {number}"));
            return false;
        }
    };
    set_result(kernel, tid, result);
    false
}

fn set_result(kernel: &mut Kernel, tid: Tid, result: u64) {
    kernel
        .threads
        .get_mut(&tid)
        .expect("calling thread exists")
        .registers
        .general[RAX] = result;
}

/// Leaves `rax` as a thread stopped inside a system call shows it.
fn inside_call(kernel: &mut Kernel, tid: Tid) {
    set_result(kernel, tid, ENOSYS_RESULT);
}

const fn failure(errno: Errno) -> u64 {
    (-(errno as i64)).cast_unsigned()
}

/// `write(fd, buffer, count)` to standard output or error, which the
/// simulation captures. Returns the result as the register holds it.
fn write(kernel: &mut Kernel, tid: Tid, fd: u64, buffer: u64, count: u64) -> u64 {
    if fd != 1 && fd != 2 {
        return failure(Errno::EBADF);
    }
    let group = kernel.threads[&tid].tgid;
    let process = &kernel.processes[&group];
    let Some(bytes) = process.space.read_user(buffer, count) else {
        return failure(Errno::EFAULT);
    };
    let root = process.root;
    kernel
        .outputs
        .get_mut(&root)
        .expect("a launched process has an output")
        .extend_from_slice(&bytes);
    count
}

/// `clone(flags, stack)` creating a thread. The child starts where its
/// creator returns from the call, on `stack`, with `rax` zero. A creator
/// tracing clones stops at `PTRACE_EVENT_CLONE` inside the call, and the
/// child, traced with the creator's options, first stops for a SIGSTOP
/// nobody sent (K-CLONE-1).
fn clone(kernel: &mut Kernel, parent: Tid, flags: u64, stack: u64) {
    if flags != THREAD_FLAGS {
        kernel.gap(format!("clone with flags {flags:#x}"));
        return;
    }
    let creator = &kernel.threads[&parent];
    // K-CLONE-1, K-SEIZE-1: the thread of a creator tracing clones is
    // traced too, and first stops for SIGSTOP, or for an interrupt when its
    // creator was seized. Any other creator's thread runs untraced.
    let traced = creator.traced() && creator.options.trace_clone;
    let seized = traced && creator.seized();
    let child = kernel.allocate_tid();
    let shadow = kernel.new_shadow();
    let creator = &kernel.threads[&parent];
    let mut registers = creator.registers;
    registers.general[RAX] = 0;
    if stack != 0 {
        registers.general[RSP] = stack;
    }
    let mut pending = Pending::default();
    if traced && !seized {
        pending.insert(SigInfo {
            signal: SIGSTOP,
            code: SI_USER,
            pid: 0,
            address: 0,
        });
    }
    let thread = Thread {
        tid: child,
        tgid: creator.tgid,
        tracing: if seized {
            Tracing::Seized { interrupted: true }
        } else if traced {
            Tracing::Attached
        } else {
            Tracing::Untraced
        },
        registers,
        // The child returns from the call it was created in.
        orig_rax: SYS_CLONE,
        returning: None,
        state: State::Running,
        options: if traced {
            creator.options
        } else {
            Options::default()
        },
        pending,
        single_step: false,
        report: None,
        trapped_at: None,
        last_trap: None,
        retired: 0,
        // The thread begins where its creator returns from the call, but
        // none of its creator's calls is on its stack.
        shadow,
        // K-DR-3: no slot armed, though DR7 reads as the creator's.
        debug: creator.debug.inherited(),
        // Perf breakpoints follow new threads.
        debug_held: creator.debug_held,
    };
    kernel.threads.insert(child, thread);
    kernel.happenings.push(Happening::Cloned { parent, child });
    let result = u64::try_from(child).expect("thread ids are positive");
    if traced {
        kernel.event_stop(parent, libc::PTRACE_EVENT_CLONE, result, result);
    } else {
        set_result(kernel, parent, result);
    }
}
