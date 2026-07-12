#include <pthread.h>
#include <stdint.h>

enum { ITERATIONS = 64 };

static uint64_t completed;

__attribute__((noinline)) static void churn_breakpoint(void) {
    completed += 1;
}

static void *worker(void *argument) {
    (void)argument;
    churn_breakpoint();
    return NULL;
}

int main(void) {
    for (int iteration = 0; iteration < ITERATIONS; ++iteration) {
        pthread_t thread;
        if (pthread_create(&thread, NULL, worker, NULL) != 0) {
            return 2;
        }
        if (pthread_join(thread, NULL) != 0) {
            return 3;
        }
    }
    return completed == ITERATIONS ? 0 : 4;
}
