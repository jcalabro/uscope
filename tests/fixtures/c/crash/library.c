#include <stdint.h>

volatile int32_t crash_library_value = 321;
_Thread_local volatile int32_t crash_library_tls = 654;

__attribute__((visibility("default"), noinline)) int32_t crash_library_touch(int32_t worker) {
    crash_library_tls = 654 + worker;
    return crash_library_value + crash_library_tls;
}
