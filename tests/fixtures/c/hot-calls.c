// Threads that call one function as fast as they can until released, so a
// breakpoint edited while they run races with their traps.
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>

enum { THREADS = 4 };

volatile int hot_stop;
_Atomic uint64_t hot_count;

__attribute__((noinline)) void hot_function(void) {
    atomic_fetch_add_explicit(&hot_count, 1, memory_order_relaxed);
}

static void *caller(void *argument) {
    (void)argument;
    while (!hot_stop) {
        hot_function();
    }
    return NULL;
}

int main(void) {
    pthread_t threads[THREADS];
    for (int index = 0; index < THREADS; ++index) {
        if (pthread_create(&threads[index], NULL, caller, NULL) != 0) {
            return 2;
        }
    }
    for (int index = 0; index < THREADS; ++index) {
        pthread_join(threads[index], NULL);
    }
    return 0;
}
