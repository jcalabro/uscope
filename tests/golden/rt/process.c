#include "process.h"
#include "thread.h"

enum {
    SYS_GETPID = 39,
    SYS_FORK = 57,
    SYS_WAIT4 = 61,
    SYS_GETPPID = 110,
    WNOHANG = 1,
};

static i64 rt_syscall3(i64 number, i64 first, i64 second, i64 third) {
    i64 result;
    // wait4's fourth argument, the resource usage, is null.
    register i64 fourth __asm__("r10") = 0;
    __asm__ volatile("syscall"
                     : "=a"(result)
                     : "a"(number), "D"(first), "S"(second), "d"(third), "r"(fourth)
                     : "rcx", "r11", "memory");
    return result;
}

long rt_fork(void) {
    return rt_syscall3(SYS_FORK, 0, 0, 0);
}

int rt_wait(long pid) {
    int status = 0;
    for (;;) {
        i64 reaped = rt_syscall3(SYS_WAIT4, pid, (i64)&status, WNOHANG);
        if (reaped == pid) {
            return status;
        }
        if (reaped < 0) {
            rt_exit_group(125);
        }
        rt_yield();
    }
}

long rt_self(void) {
    return rt_syscall3(SYS_GETPID, 0, 0, 0);
}

long rt_parent(void) {
    return rt_syscall3(SYS_GETPPID, 0, 0, 0);
}
