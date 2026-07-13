#include <stdint.h>

volatile uint64_t unwind_sink;

__attribute__((noinline)) static uint64_t deepest(uint64_t value) {
    unwind_sink = value;
    return unwind_sink + 1;
}

__attribute__((noinline)) static uint64_t middle(uint64_t value) {
    uint64_t result = deepest(value + 1);
    return result + unwind_sink;
}

__attribute__((noinline)) static uint64_t outer(uint64_t value) {
    uint64_t result = middle(value + 1);
    return result + unwind_sink;
}

int main(int argc, char **argv) {
    (void)argv;
    return outer((uint64_t)argc) == 11 ? 0 : 1;
}
