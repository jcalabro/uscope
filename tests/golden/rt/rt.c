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

#ifdef __PIE__
// A static position-independent executable has no loader to relocate it,
// so it applies its own relative relocations before anything reads a
// pointer they cover. It is linked at zero, so its header's address is
// where it loaded.
typedef struct {
    u64 offset;
    u64 info;
    i64 addend;
} rt_rela;

extern const char __ehdr_start[] __attribute__((visibility("hidden")));
extern const u64 _DYNAMIC[] __attribute__((visibility("hidden")));

enum {
    DT_NULL = 0,
    DT_RELA = 7,
    DT_RELASZ = 8,
    R_X86_64_RELATIVE = 8,
};

static void rt_relocate(void) {
    u64 base = (u64)__ehdr_start;
    const rt_rela *relocations = 0;
    u64 size = 0;
    for (const u64 *entry = _DYNAMIC; entry[0] != DT_NULL; entry += 2) {
        if (entry[0] == DT_RELA) {
            relocations = (const rt_rela *)(base + entry[1]);
        } else if (entry[0] == DT_RELASZ) {
            size = entry[1];
        }
    }
    for (u64 index = 0; index < size / sizeof(rt_rela); index++) {
        if ((relocations[index].info & 0xffffffff) != R_X86_64_RELATIVE) {
            rt_exit_group(126);
        }
        *(u64 *)(base + relocations[index].offset) = base + (u64)relocations[index].addend;
    }
}
#endif

void rt_start(i64 *stack) {
#ifdef __PIE__
    rt_relocate();
#endif
    int argc = (int)stack[0];
    char **argv = (char **)(stack + 1);
    rt_exit_group(main(argc, argv));
}
