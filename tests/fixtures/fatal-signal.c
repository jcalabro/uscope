#include <stdint.h>

__attribute__((noinline)) static void fault(void) {
    volatile uint64_t *address = (volatile uint64_t *)(uintptr_t)1;
    *address = 42;
}

int main(void) {
    fault();
}
