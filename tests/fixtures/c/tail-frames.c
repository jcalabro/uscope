// Functions that leave by tail calls, whose frames a backtrace shows from
// the calls that the debug information describes. Each function's tail
// call and the line of each call are marked.
#include <stdio.h>

// Kept from interprocedural analysis, which would clone a function for the
// constant it is called with, or let its caller keep values in registers
// it does not clobber, which no unwinder can tell.
#if defined(__clang__)
#define OPAQUE __attribute__((noinline))
#else
#define OPAQUE __attribute__((noipa))
#endif

volatile int sink;

OPAQUE void reached(void) {
    __asm__ volatile("" ::: "memory");
}

OPAQUE int leaf(int value) {
    sink = value;
    reached(); // frames: leaf
    return sink + 1;
}

// One chain of tail calls reaches leaf from top: top, middle, leaf.
OPAQUE int middle(int value) {
    sink = value;
    return leaf(value * 2); // frames: middle
}

OPAQUE int top(int value) {
    sink = value;
    return middle(value + 1); // frames: top
}

// Two chains reach leaf from either: directly, or through middle.
OPAQUE int either(int value) {
    sink = value;
    if (value & 1) {
        return middle(value); // frames: either middle
    }
    return leaf(value + 3); // frames: either leaf
}

int main(void) {
    int total = top(5); // frames: call top
    total += either(8); // frames: call either
    printf("%d\n", total);
    return 0;
}
