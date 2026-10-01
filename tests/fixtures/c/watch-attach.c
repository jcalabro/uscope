#define _GNU_SOURCE

#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/prctl.h>
#include <unistd.h>

enum { ITERATIONS = 200 };

volatile uint64_t attach_watched;

static void write_half(void) {
    for (int iteration = 0; iteration < ITERATIONS / 2; ++iteration) {
        attach_watched += 1;
    }
}

// Writes from a thread created after any debugger attached.
static void *writer(void *argument) {
    (void)argument;
    write_half();
    return NULL;
}

// Announces the watched word's address, waits to be released, then writes it
// repeatedly from the main thread and from a new thread. Exits 0 only if every
// write happened.
int main(void) {
    char release;

    if (prctl(PR_SET_PTRACER, PR_SET_PTRACER_ANY) == -1) {
        return 1;
    }
    if (printf("READY %p\n", (void *)&attach_watched) < 0 || fflush(stdout) != 0) {
        return 2;
    }
    if (read(STDIN_FILENO, &release, 1) != 1) {
        return 3;
    }
    write_half();
    pthread_t thread;
    if (pthread_create(&thread, NULL, writer, NULL) != 0 || pthread_join(thread, NULL) != 0) {
        return 4;
    }
    return attach_watched == ITERATIONS ? 0 : 5;
}
