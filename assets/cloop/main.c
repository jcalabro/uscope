#include <unistd.h>
#include <stdio.h>

int main() {
    pid_t pid = getpid();
    unsigned long long ndx = 0;
    while (1) {
        printf("c looping (pid %d): %llu\n", pid, ndx);
        fflush(stdout);
        ndx++;
        sleep(1);
    }

    return 0;
}
