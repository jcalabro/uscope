#include <stdint.h>

volatile uint64_t uscope_value = UINT64_C(0x1122334455667788);

__attribute__((noinline)) uint64_t breakpoint_target(void) {
    return uscope_value;
}

int main(void) {
    uint64_t first = breakpoint_target();
    uint64_t second = breakpoint_target();
    return first == uscope_value && second == uscope_value ? 0 : 1;
}
