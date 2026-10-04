//! The system calls the golden runtime makes. Any other is a model gap.

use nix::errno::Errno;

use super::{ExitStatus, Kernel, Tid};
use crate::sim::cpu::{RAX, RDI, RDX, RSI};

const SYS_WRITE: u64 = 1;
const SYS_EXIT_GROUP: u64 = 231;

/// Serves the system call a thread just entered with `syscall`, leaving
/// its result in `rax`.
pub(super) fn serve(kernel: &mut Kernel, tid: Tid) {
    let registers = kernel.threads[&tid].registers;
    let argument = |index: usize| registers.general[index];
    match registers.general[RAX] {
        SYS_WRITE => {
            let result = write(kernel, tid, argument(RDI), argument(RSI), argument(RDX));
            kernel
                .threads
                .get_mut(&tid)
                .expect("calling thread exists")
                .registers
                .general[RAX] = result;
        }
        SYS_EXIT_GROUP => {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "exit_group takes an int, the low half of the register"
            )]
            let code = argument(RDI) as i32;
            let group = kernel.threads[&tid].tgid;
            kernel.kill_process(group, ExitStatus::Code(code));
            // The caller is already in the kernel, and goes straight on to
            // its exit.
            kernel.reach_exit(tid, ExitStatus::Code(code));
        }
        number => kernel.gap(format!("system call {number}")),
    }
}

/// `write(fd, buffer, count)` to standard output or error, which the
/// simulation captures. Returns the result as the register holds it.
fn write(kernel: &mut Kernel, tid: Tid, fd: u64, buffer: u64, count: u64) -> u64 {
    let failure = |errno: Errno| (-(errno as i64)).cast_unsigned();
    if fd != 1 && fd != 2 {
        return failure(Errno::EBADF);
    }
    let group = kernel.threads[&tid].tgid;
    let process = kernel
        .processes
        .get_mut(&group)
        .expect("a thread's process exists");
    let Some(bytes) = process.space.read_user(buffer, count) else {
        return failure(Errno::EFAULT);
    };
    process.output.extend_from_slice(&bytes);
    count
}
