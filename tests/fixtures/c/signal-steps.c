// Steps through code that signals itself with a raw system call, so a step
// is certain to cross the signal's arrival. Its handler must run without a
// step stopping in it.
#define _GNU_SOURCE

#include <signal.h>
#include <sys/syscall.h>
#include <unistd.h>

volatile sig_atomic_t handled;
static pid_t process;
static pid_t thread;

static void handler(int signal) {
    (void)signal;
    handled += 1;
}

__attribute__((noinline)) long sum_to(long count) {
    long total = 0;
    for (long index = 0; index < count; ++index) {
        total += index;
        if (index == 2) {
            long result;
            __asm__ volatile("syscall" // signals this thread
                             : "=a"(result)
                             : "a"((long)SYS_tgkill), "D"((long)process), "S"((long)thread),
                               "d"((long)SIGUSR2)
                             : "rcx", "r11", "memory");
            (void)result;
        }
    }
    return total;
}

int main(void) {
    struct sigaction action = {.sa_handler = handler};
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGUSR2, &action, NULL) != 0) {
        return 100;
    }
    process = getpid();
    thread = gettid();
    long first = sum_to(5); // the stepped call
    long second = sum_to(5);
    long third = sum_to(5);
    return first == 10 && second == 10 && third == 10 && handled == 3 ? 0 : 1;
}
