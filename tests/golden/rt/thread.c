#include "thread.h"

enum {
    SYS_SCHED_YIELD = 24,
    SYS_EXIT = 60,
};

// The flags of a thread sharing everything with its creator, as
// pthread_create passes them, without the TLS and tid bookkeeping.
#define THREAD_FLAGS 0x50f00 // VM | FS | FILES | SIGHAND | THREAD | SYSVSEM

// rt_clone(flags, stack_top, fn, arg). The new thread starts after the
// system call on stack_top, which holds a zero return address: unwinders
// stop there instead of reading the creator's frame. It then runs
// fn(arg) from rt_thread_start, which its CFI marks as the outermost frame,
// and exits with status 0 when fn returns.
__asm__(".text\n"
        ".global rt_clone\n"
        ".type rt_clone, @function\n"
        "rt_clone:\n"
        "    .cfi_startproc\n"
        "    mov %rdx, %r8\n"
        "    mov %rcx, %r9\n"
        "    xor %edx, %edx\n"
        "    xor %r10d, %r10d\n"
        "    mov $56, %eax\n"
        "    syscall\n"
        "    test %rax, %rax\n"
        "    jz rt_thread_start\n"
        "    ret\n"
        "    .cfi_endproc\n"
        ".size rt_clone, . - rt_clone\n"
        "\n"
        ".type rt_thread_start, @function\n"
        "rt_thread_start:\n"
        "    .cfi_startproc\n"
        "    .cfi_undefined rip\n"
        "    xor %ebp, %ebp\n"
        "    mov %r9, %rdi\n"
        "    call *%r8\n"
        "    xor %edi, %edi\n"
        "    mov $60, %eax\n"
        "    syscall\n"
        "    hlt\n"
        "    .cfi_endproc\n"
        ".size rt_thread_start, . - rt_thread_start\n");

long rt_clone(u64 flags, u64 *stack_top, rt_thread_fn fn, void *arg);

static i64 rt_syscall1(i64 number, i64 first) {
    i64 result;
    __asm__ volatile("syscall" : "=a"(result) : "a"(number), "D"(first) : "rcx", "r11", "memory");
    return result;
}

long rt_spawn(rt_thread_fn fn, void *arg, char *stack, u64 size) {
    // Volatile keeps clang from storing both words with one SSE move.
    volatile u64 *top = (volatile u64 *)(((u64)stack + size) & ~(u64)15);
    // A zero return address, and a word that keeps the stack aligned for
    // the call to fn.
    *--top = 0;
    *--top = 0;
    return rt_clone(THREAD_FLAGS, (u64 *)top, fn, arg);
}

_Noreturn void rt_exit(int code) {
    for (;;) {
        rt_syscall1(SYS_EXIT, code);
    }
}

void rt_yield(void) {
    rt_syscall1(SYS_SCHED_YIELD, 0);
}
