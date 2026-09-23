#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <unistd.h>

static _Atomic int ready;
static _Atomic int release_workers;

static void *worker(void *unused) {
    (void)unused;
    atomic_fetch_add_explicit(&ready, 1, memory_order_release);
    while (atomic_load_explicit(&release_workers, memory_order_acquire) == 0) {
        sched_yield();
    }
    return NULL;
}

int main(void) {
    pthread_t first;
    pthread_t second;
    char release;
    static const char ready_message[] = "READY\n";

    if (pthread_create(&first, NULL, worker, NULL) != 0 ||
        pthread_create(&second, NULL, worker, NULL) != 0) {
        return 2;
    }
    while (atomic_load_explicit(&ready, memory_order_acquire) != 2) {
        sched_yield();
    }
    if (write(STDOUT_FILENO, ready_message, sizeof(ready_message) - 1) !=
        (ssize_t)(sizeof(ready_message) - 1)) {
        return 3;
    }
    if (read(STDIN_FILENO, &release, 1) != 1) {
        return 4;
    }
    atomic_store_explicit(&release_workers, 1, memory_order_release);
    if (pthread_join(first, NULL) != 0 || pthread_join(second, NULL) != 0) {
        return 5;
    }
    return 0;
}
