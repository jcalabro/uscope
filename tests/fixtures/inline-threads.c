#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>

static atomic_int workers_ready;
static atomic_int release_workers;
static atomic_int inline_thread_sink;

static __attribute__((always_inline)) inline int thread_leaf(int value) {
    int result = value * 2 + 1;

    atomic_fetch_add_explicit(&inline_thread_sink, result, memory_order_relaxed);
    return result;
}

__attribute__((noinline)) static int thread_caller(int value) {
    int result = thread_leaf(value);

    atomic_fetch_add_explicit(&inline_thread_sink, value, memory_order_relaxed);
    return result;
}

static void *worker(void *argument) {
    int value = (int)(intptr_t)argument;

    atomic_fetch_add_explicit(&workers_ready, 1, memory_order_release);
    while (atomic_load_explicit(&release_workers, memory_order_acquire) == 0) {
    }

    return (void *)(intptr_t)thread_caller(value);
}

int main(void) {
    pthread_t first;
    pthread_t second;

    if (pthread_create(&first, NULL, worker, (void *)(intptr_t)2) != 0 ||
        pthread_create(&second, NULL, worker, (void *)(intptr_t)3) != 0) {
        return 2;
    }
    while (atomic_load_explicit(&workers_ready, memory_order_acquire) != 2) {
    }
    atomic_store_explicit(&release_workers, 1, memory_order_release);

    void *first_result;
    void *second_result;
    if (pthread_join(first, &first_result) != 0 || pthread_join(second, &second_result) != 0) {
        return 3;
    }

    return (intptr_t)first_result + (intptr_t)second_result == 12 ? 0 : 1;
}
