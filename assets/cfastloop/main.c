#include <unistd.h>
#include <stdio.h>

int main() {
    pid_t pid = getpid();
    unsigned long long ndx = 0;
    while (1) {
        if ((ndx % 10000000) == 0) {
            printf("c fast looping (pid %d): %llu\n", pid, ndx);
            fflush(stdout);
        }

        ndx++; // simulator test sets a breakpoint here
    }

    return 0;
}
