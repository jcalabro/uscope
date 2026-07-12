#include <stdint.h>

volatile uint64_t step_counter;

__attribute__((noinline)) static void step_forever(void) {
    for (;;) {
        step_counter += 1;
    }
}

int main(void) {
    step_forever();
}
