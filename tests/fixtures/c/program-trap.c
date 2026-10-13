// Executes a breakpoint instruction of its own, as a debugger-aware program
// does to stop itself. The debugger reports it as the program's, not one of
// its own, and the program resumes after the instruction.

#include <stdio.h>

int main(void) {
    puts("before");
    __asm__ volatile("int3");
    puts("after");
    return 0;
}
