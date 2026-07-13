#include <stdint.h>

volatile int boundary_input = -1;
volatile int boundary_sink;

__attribute__((noinline)) int marked_returns(int value) {
    volatile int frame[40];
    frame[0] = value;
    if (frame[0] < 0) {
        boundary_sink = 11;
        return -frame[0];
    }

    boundary_sink = 22;
    return frame[0] + 1;
}

__attribute__((noinline)) int no_prologue(int value) {
    boundary_sink = value;
    return value + 1;
}

static __attribute__((always_inline)) inline int inline_adjust(int value) {
    int adjusted = value + 7;
    boundary_sink = adjusted;
    return adjusted * 2;
}

int main(void) {
    int inlined = inline_adjust(boundary_input);
    int first = marked_returns(boundary_input);
    boundary_input = 4;
    int second = marked_returns(boundary_input);
    int third = no_prologue(boundary_input);
    return inlined == 12 && first == 1 && second == 5 && third == 5 ? 0 : 1;
}
