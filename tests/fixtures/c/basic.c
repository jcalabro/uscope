#include <stdint.h>

volatile uint64_t microscope_value = UINT64_C(0x1122334455667788);

__attribute__((noinline)) uint64_t breakpoint_target(void) {
    return microscope_value;
}

int main(void) {
    uint64_t first = breakpoint_target();
    uint64_t second = breakpoint_target();
    return first == microscope_value && second == microscope_value ? 0 : 1;
}
