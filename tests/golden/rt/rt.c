#include "rt.h"

// The kernel enters with the stack pointer at argc. The CFI marks this as
// the outermost frame, so unwinders stop here instead of guessing.
__asm__(".text\n"
        ".global _start\n"
        ".type _start, @function\n"
        "_start:\n"
        "    .cfi_startproc\n"
        "    .cfi_undefined rip\n"
        "    xor %ebp, %ebp\n"
        "    mov %rsp, %rdi\n"
        "    and $-16, %rsp\n"
        "    call rt_start\n"
        "    hlt\n"
        "    .cfi_endproc\n"
        ".size _start, . - _start\n");

enum {
    SYS_WRITE = 1,
    SYS_EXIT_GROUP = 231,
};

static i64 rt_syscall3(i64 number, i64 first, i64 second, i64 third) {
    i64 result;
    __asm__ volatile("syscall"
                     : "=a"(result)
                     : "a"(number), "D"(first), "S"(second), "d"(third)
                     : "rcx", "r11", "memory");
    return result;
}

void rt_write(int fd, const char *bytes, u64 count) {
    while (count > 0) {
        i64 written = rt_syscall3(SYS_WRITE, fd, (i64)bytes, (i64)count);
        if (written <= 0) {
            rt_exit_group(127);
        }
        bytes += written;
        count -= (u64)written;
    }
}

void rt_print(const char *text) {
    u64 length = 0;
    while (text[length] != 0) {
        length++;
    }
    rt_write(1, text, length);
}

void rt_print_u64(u64 value) {
    char digits[20];
    int start = sizeof digits;
    do {
        digits[--start] = (char)('0' + value % 10);
        value /= 10;
    } while (value != 0);
    rt_write(1, digits + start, sizeof digits - (u64)start);
}

u64 rt_parse_u64(const char *text) {
    u64 value = 0;
    for (; *text >= '0' && *text <= '9'; text++) {
        value = value * 10 + (u64)(*text - '0');
    }
    return value;
}

_Noreturn void rt_exit_group(int code) {
    for (;;) {
        rt_syscall3(SYS_EXIT_GROUP, code, 0, 0);
    }
}

void rt_start(i64 *stack) {
    int argc = (int)stack[0];
    char **argv = (char **)(stack + 1);
    rt_exit_group(main(argc, argv));
}
