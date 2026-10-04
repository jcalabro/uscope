#include <pthread.h>
#include <sched.h>

static volatile int stop;

static void *worker(void *unused) {
    (void)unused;
    while (!stop) {
        sched_yield();
    }
    return NULL;
}

// The main thread exits alone while the worker runs on. Linux reports the
// main thread's exit only once every other thread is gone.
int main(void) {
    pthread_t thread;

    if (pthread_create(&thread, NULL, worker, NULL) != 0) {
        return 2;
    }
    pthread_exit(NULL);
}
