#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <sys/prctl.h>
#include <time.h>

// The main thread creates threads without pause, so an attach is likely to
// seize it inside clone. Each thread outlives an attach, so one that started
// untraced is still there to be found.
enum { LIVE_WORKERS = 128 };

static _Atomic int live;

static void *worker(void *argument) {
    (void)argument;
    const struct timespec lifetime = {.tv_sec = 0, .tv_nsec = 20 * 1000 * 1000};
    nanosleep(&lifetime, NULL);
    atomic_fetch_sub_explicit(&live, 1, memory_order_relaxed);
    return NULL;
}

int main(void) {
    if (prctl(PR_SET_PTRACER, PR_SET_PTRACER_ANY) == -1) {
        return 1;
    }
    pthread_attr_t attributes;
    if (pthread_attr_init(&attributes) != 0 ||
        pthread_attr_setdetachstate(&attributes, PTHREAD_CREATE_DETACHED) != 0 ||
        pthread_attr_setstacksize(&attributes, 64 * 1024) != 0) {
        return 2;
    }
    for (;;) {
        if (atomic_load_explicit(&live, memory_order_relaxed) >= LIVE_WORKERS) {
            sched_yield();
            continue;
        }
        atomic_fetch_add_explicit(&live, 1, memory_order_relaxed);
        pthread_t thread;
        if (pthread_create(&thread, &attributes, worker, NULL) != 0) {
            return 3;
        }
    }
}
