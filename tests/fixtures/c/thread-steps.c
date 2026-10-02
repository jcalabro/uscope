// Threads that a step must let run. In the default mode the main thread
// waits in pthread_join for a worker that sleeps first; with `spin` it runs
// until the debugger sets `main_released` instead. With `race`, a racer
// thread keeps running the same functions the main thread steps through, so
// it reaches every internal breakpoint of the main thread's steps.
#define _GNU_SOURCE

#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>

volatile int64_t thread_steps_sink;
volatile int main_released;
_Atomic int64_t racer_rounds;
static _Atomic int stop_racing;
static _Atomic int worker_finished;

__attribute__((noinline)) void worker_reached(void) {
    thread_steps_sink += 1;
}

__attribute__((noinline)) int64_t shared_work(int64_t seed) {
    int64_t value = seed * 2;
    value += 3;
    thread_steps_sink = value;
    return value;
}

__attribute__((noinline)) int64_t shared_caller(int64_t seed) {
    int64_t result = shared_work(seed);
    thread_steps_sink = result;
    return result;
}

static void *sleepy_worker(void *argument) {
    (void)argument;
    pthread_setname_np(pthread_self(), "sleepy-worker");
    usleep(100 * 1000);
    worker_reached();
    atomic_store(&worker_finished, 1);
    return NULL;
}

static void *racer(void *argument) {
    (void)argument;
    while (!atomic_load(&stop_racing)) {
        shared_caller(atomic_fetch_add(&racer_rounds, 1));
        usleep(10);
    }
    return NULL;
}

int main(int argc, char **argv) {
    pthread_t thread;
    if (argc > 1 && strcmp(argv[1], "race") == 0) {
        if (pthread_create(&thread, NULL, racer, NULL) != 0) {
            return 2;
        }
        while (atomic_load(&racer_rounds) < 10) {
        }
        int64_t result = shared_caller(7); // main's shared call
        atomic_store(&stop_racing, 1);
        pthread_join(thread, NULL);
        return result == 17 ? 0 : 3;
    }
    if (pthread_create(&thread, NULL, sleepy_worker, NULL) != 0) {
        return 2;
    }
    if (argc > 1 && strcmp(argv[1], "spin") == 0) {
        while (!main_released) {
        }
    }
    pthread_join(thread, NULL); // joins the sleepy worker
    return atomic_load(&worker_finished) ? 0 : 4;
}
