#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>

static _Atomic int second_ready;
static _Atomic int release_second;

__attribute__((noinline)) static void *second_worker(void *argument) {
    (void)argument;
    int thread_value = 202;
    atomic_store_explicit(&second_ready, 1, memory_order_release);
    while (!atomic_load_explicit(&release_second, memory_order_acquire)) {
        __asm__ volatile("" : : "r"(thread_value) : "memory");
    }
    return (void *)(uintptr_t)thread_value;
}

__attribute__((noinline)) static void *first_worker(void *argument) {
    (void)argument;
    int thread_value = 101;
    while (!atomic_load_explicit(&second_ready, memory_order_acquire)) {
    }
    return (void *)(uintptr_t)thread_value;
}

int main(void) {
    pthread_t first;
    pthread_t second;
    if (pthread_create(&second, NULL, second_worker, NULL) != 0 ||
        pthread_create(&first, NULL, first_worker, NULL) != 0) {
        return 2;
    }
    void *first_result;
    void *second_result;
    if (pthread_join(first, &first_result) != 0) {
        return 3;
    }
    atomic_store_explicit(&release_second, 1, memory_order_release);
    if (pthread_join(second, &second_result) != 0) {
        return 4;
    }
    return (uintptr_t)first_result == 101 && (uintptr_t)second_result == 202 ? 0 : 1;
}
