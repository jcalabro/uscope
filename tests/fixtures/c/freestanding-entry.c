// A program without a C library. Its entry point, like crt1's, has
// call-frame information marking it outermost but no line information, so
// stepping begins in code no debug information describes.
__asm__(".globl _start\n"
        "_start:\n"
        ".cfi_startproc\n"
        ".cfi_undefined rip\n"
        "\txor %ebp, %ebp\n"
        "\tcall main\n"
        "\tmov %eax, %edi\n"
        "\tmov $231, %eax\n"
        "\tsyscall\n"
        ".cfi_endproc\n");

static volatile long total;

int main(void) {
    total = 1; // FIRST_STATEMENT
    total += 2;
    return (int)total;
}
