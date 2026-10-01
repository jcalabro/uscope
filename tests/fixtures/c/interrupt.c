// Raises SIGINT with its default disposition, as a terminal Ctrl-C would.
// The process survives to exit 0 only if the debugger discards the signal.

#include <signal.h>

int main(void) {
    raise(SIGINT);
    return 0;
}
