#include <stdint.h>

volatile uint64_t step_counter;
volatile uint64_t step_release;

__attribute__((noinline)) static void step_forever(void) {
    while (!step_release) {
        step_counter += 1;
    }
}

int main(void) {
    step_forever();
}
