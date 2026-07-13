#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>

_Atomic uint64_t thread_counter;
static _Atomic int workers_ready;
static _Atomic int release_workers;

__attribute__((noinline)) static void worker_breakpoint(uint64_t value) {
    atomic_fetch_add_explicit(&thread_counter, value, memory_order_relaxed);
}

static void *worker(void *argument) {
    uint64_t value = (uint64_t)(uintptr_t)argument;

    atomic_fetch_add_explicit(&workers_ready, 1, memory_order_release);
    while (atomic_load_explicit(&release_workers, memory_order_acquire) == 0) {
        sched_yield();
    }

    worker_breakpoint(value);
    for (uint64_t index = 0; index < 100000; ++index) {
        atomic_fetch_add_explicit(&thread_counter, 1, memory_order_relaxed);
    }
    return NULL;
}

int main(void) {
    pthread_t first;
    pthread_t second;

    if (pthread_create(&first, NULL, worker, (void *)(uintptr_t)1) != 0 ||
        pthread_create(&second, NULL, worker, (void *)(uintptr_t)2) != 0) {
        return 2;
    }
    while (atomic_load_explicit(&workers_ready, memory_order_acquire) != 2) {
        sched_yield();
    }
    atomic_store_explicit(&release_workers, 1, memory_order_release);

    if (pthread_join(first, NULL) != 0 || pthread_join(second, NULL) != 0) {
        return 3;
    }
    return atomic_load_explicit(&thread_counter, memory_order_relaxed) == 200003 ? 0 : 4;
}
