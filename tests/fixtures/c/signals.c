#define _POSIX_C_SOURCE 200809L

#include <signal.h>
#include <stddef.h>

static volatile sig_atomic_t usr1_handled;
static volatile sig_atomic_t trap_handled;

static void handle_usr1(int signal) {
    (void)signal;
    usr1_handled = 1;
}

static void handle_trap(int signal) {
    (void)signal;
    trap_handled = 1;
}

__attribute__((noinline)) static void signal_point(void) {
    raise(SIGUSR1);
    raise(SIGTRAP);
}

int main(void) {
    struct sigaction usr1 = {.sa_handler = handle_usr1};
    struct sigaction trap = {.sa_handler = handle_trap};

    sigemptyset(&usr1.sa_mask);
    sigemptyset(&trap.sa_mask);
    if (sigaction(SIGUSR1, &usr1, NULL) != 0 || sigaction(SIGTRAP, &trap, NULL) != 0) {
        return 2;
    }

    signal_point();
    return usr1_handled && trap_handled ? 0 : 42;
}
