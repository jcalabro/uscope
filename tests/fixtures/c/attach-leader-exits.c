#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static volatile int released;

static void *worker(void *unused) {
    char byte;

    (void)unused;
    while (!released) {
        sched_yield();
    }
    if (read(STDIN_FILENO, &byte, 1) != 1) {
        exit(4);
    }
    exit(7);
}

// Once attached, the main thread exits alone on the first byte of input;
// the worker then exits the process with status 7 on the second.
int main(void) {
    pthread_t thread;
    char byte;

    if (pthread_create(&thread, NULL, worker, NULL) != 0) {
        return 2;
    }
    printf("READY\n");
    fflush(stdout);
    if (read(STDIN_FILENO, &byte, 1) != 1) {
        return 3;
    }
    released = 1;
    pthread_exit(NULL);
}
