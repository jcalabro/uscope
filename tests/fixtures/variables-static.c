#include <stdint.h>

static volatile int32_t static_sink;

__attribute__((noinline)) static int32_t inspect_static(void) {
    static volatile int32_t static_value = 73;
    static_sink = static_value;
    return static_value;
}

int main(void) {
    return inspect_static() == 73 ? 0 : 1;
}
