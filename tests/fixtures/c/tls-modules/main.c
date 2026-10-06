// Three threads, each with its own copy of TLS in the executable, a linked
// library, and a plugin loaded while one thread already runs. Every thread
// records where its copies are, so a debugger's addresses can be checked
// against the program's own. A static build has no plugin and links the
// library into the executable. With the argument `abort`, the program
// aborts where it would stop, leaving a core dump.

#define _GNU_SOURCE

#include <pthread.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#ifndef STATIC_BUILD
#include <dlfcn.h>
#endif

enum { THREADS = 3 };

struct tls_addresses {
    volatile int32_t *main;
    volatile int64_t *zero;
    volatile int32_t *library;
    volatile int64_t *plugin;
};

_Thread_local volatile int32_t main_tls = 100;
_Thread_local volatile int64_t main_zero_tls;
extern _Thread_local volatile int32_t library_tls;

// Indexed by thread: 0 is the main thread, 1 starts before the plugin is
// loaded, and 2 after.
struct tls_addresses tls_addresses[THREADS];

static volatile int64_t *(*plugin_tls_address)(void);
static pthread_barrier_t loaded;
static pthread_barrier_t ready;
static pthread_barrier_t release;
static int aborting;

__attribute__((noinline)) static void tls_stop(void) {
    if (aborting) {
        abort();
    }
}

static void record(int index) {
    main_tls += index;
    main_zero_tls = 200 + index;
    library_tls += index;
    tls_addresses[index].main = &main_tls;
    tls_addresses[index].zero = &main_zero_tls;
    tls_addresses[index].library = &library_tls;
    if (plugin_tls_address != NULL) {
        volatile int64_t *plugin = plugin_tls_address();
        *plugin += index;
        tls_addresses[index].plugin = plugin;
    }
}

static void *worker(void *argument) {
    int index = (int)(intptr_t)argument;
    if (index == 1) {
        pthread_barrier_wait(&loaded);
    }
    record(index);
    pthread_barrier_wait(&ready);
    pthread_barrier_wait(&release);
    return NULL;
}

int main(int argc, char **argv) {
    aborting = argc > 1 && strcmp(argv[1], "abort") == 0;
    pthread_t early;
    pthread_t late;
    if (pthread_barrier_init(&loaded, NULL, 2) != 0 ||
        pthread_barrier_init(&ready, NULL, THREADS) != 0 ||
        pthread_barrier_init(&release, NULL, THREADS) != 0 ||
        pthread_create(&early, NULL, worker, (void *)(intptr_t)1) != 0) {
        return 1;
    }
#ifndef STATIC_BUILD
    void *plugin = dlopen(PLUGIN, RTLD_NOW);
    if (plugin == NULL) {
        return 1;
    }
    plugin_tls_address = (volatile int64_t * (*)(void)) dlsym(plugin, "plugin_tls_address");
    if (plugin_tls_address == NULL) {
        return 1;
    }
#endif
    pthread_barrier_wait(&loaded);
    if (pthread_create(&late, NULL, worker, (void *)(intptr_t)2) != 0) {
        return 1;
    }
    record(0);
    pthread_barrier_wait(&ready);
    tls_stop();
    pthread_barrier_wait(&release);
    pthread_join(early, NULL);
    pthread_join(late, NULL);
    return 0;
}
