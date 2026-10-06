#include <stdint.h>

_Thread_local volatile int64_t plugin_tls = 400;

volatile int64_t *plugin_tls_address(void) {
    return &plugin_tls;
}
