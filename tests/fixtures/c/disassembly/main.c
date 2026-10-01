// Code whose disassembly the tests check: a direct call, a call through the
// procedure linkage table, program-counter-relative data, and hand-written
// code with data inside it (layout.S).
#include <stdio.h>

int disasm_marked_data(void);
int disasm_hidden_data(void);

int disasm_counter = 40;

__attribute__((noinline)) static int disasm_helper(int value) {
    return value + disasm_counter;
}

int main(void) {
    disasm_counter += disasm_marked_data();
    disasm_counter += disasm_hidden_data();
    printf("%d\n", disasm_helper(0));
    return 0;
}
