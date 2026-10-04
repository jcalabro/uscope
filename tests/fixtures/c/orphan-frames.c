// Functions whose callers cannot be unwound, as a debugger meets them in
// coroutine libraries and corrupted stacks.
//
// By default, `fiber_main` runs on a fresh stack it entered by a jump, so its
// call-frame information names a return-address slot just past the end of
// that stack, where nothing is mapped. With the argument `smash`, `smash`
// overwrites its own return address with a pointer into program data, as a
// stack buffer overflow would. Neither returns; each exits with status 0
// only when every value it computed is intact.
#define _GNU_SOURCE
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

static volatile long fiber_total;
static volatile long canary[2] = {7, 7};

// Exits through the system call, which needs no stack alignment and never
// returns into a broken caller.
__attribute__((noreturn)) static void exit_now(long status) {
    __asm__ volatile("syscall" : : "a"((long)SYS_exit_group), "D"(status) : "rcx", "r11", "memory");
    __builtin_unreachable();
}

__attribute__((noinline, noreturn, used)) void fiber_main(void) {
    fiber_total = 1; // FIBER_FIRST
    fiber_total += 2; // FIBER_SECOND
    fiber_total += 3; // FIBER_THIRD
    exit_now(fiber_total == 6 ? 0 : 1); // FIBER_EXIT
}

__attribute__((noreturn)) static void start_fiber(void) {
    long page = sysconf(_SC_PAGESIZE);
    char *stack = mmap(NULL, 3 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (stack == MAP_FAILED) {
        exit_now(2);
    }
    // Leaves nothing readable just past the stack's end. A PROT_NONE guard
    // would not do: ptrace reads through page protection.
    munmap(stack + 2 * page, page);
    char *top = stack + 2 * page;
    __asm__ volatile("mov %0, %%rsp\n\tjmp fiber_main" : : "r"(top) : "memory");
    __builtin_unreachable();
}

__attribute__((noinline, noreturn)) static void smash(void) {
    void **frame = __builtin_frame_address(0); // FRAME_ADDRESS
    frame[1] = (void *)&canary[1]; // OVERWRITE_RETURN
    long seen = canary[1]; // READ_CANARY
    exit_now(seen == 7 && canary[0] == 7 ? 0 : 1); // CHECK_CANARY
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "smash") == 0) {
        smash();
    }
    start_fiber();
}
