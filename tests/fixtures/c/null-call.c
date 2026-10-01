// Calls through a null function pointer, so the fault happens at an
// instruction address that no loaded module contains.
#include <stddef.h>

typedef void (*callback_fn)(void);

volatile callback_fn null_callback = NULL;

int main(void) {
    null_callback();
    return 0;
}
