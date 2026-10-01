#include <stdint.h>

__attribute__((visibility("default"), noinline)) int32_t dso_apply(int32_t (*callback)(int32_t),
                                                                   int32_t value) {
    volatile int32_t adjusted = value + 1;
    return callback(adjusted) + 1;
}
