#define _GNU_SOURCE

#include <pthread.h>
#include <stdint.h>

_Thread_local volatile int32_t tls_value;
volatile int32_t tls_sink;
static pthread_barrier_t ready;
static pthread_barrier_t release;

__attribute__((noinline)) static void tls_stop(void) {
    tls_sink = tls_value;
}

__attribute__((noinline)) static void tls_after_join(void) {
    tls_sink = tls_value;
}

static void *worker(void *argument) {
    tls_value = (int32_t)(intptr_t)argument;
    pthread_barrier_wait(&ready);
    pthread_barrier_wait(&release);
    return NULL;
}

int main(void) {
    pthread_t first;
    pthread_t second;
    if (pthread_barrier_init(&ready, NULL, 3) != 0 ||
        pthread_barrier_init(&release, NULL, 3) != 0 ||
        pthread_create(&first, NULL, worker, (void *)(intptr_t)301) != 0 ||
        pthread_create(&second, NULL, worker, (void *)(intptr_t)302) != 0) {
        return 1;
    }
    tls_value = 300;
    pthread_barrier_wait(&ready);
    tls_stop();
    pthread_barrier_wait(&release);
    pthread_join(first, NULL);
    pthread_join(second, NULL);
    tls_after_join();
    pthread_barrier_destroy(&ready);
    pthread_barrier_destroy(&release);
    return tls_sink == 300 ? 0 : 1;
}
