#include <stdint.h>

volatile int32_t dso_external = 211;
volatile int32_t *dso_pointer = &dso_external;
static volatile int32_t dso_static = 212;
static volatile int32_t module_collision;
_Thread_local volatile int32_t dso_tls = 213;

__attribute__((visibility("default"), noinline)) int32_t dso_touch(void) {
    return *dso_pointer + dso_static + dso_tls + module_collision;
}
