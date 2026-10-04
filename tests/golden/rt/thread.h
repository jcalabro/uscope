// Threads for golden programs: raw clone with a stack the caller provides,
// thread exit, yielding, and the atomics barriers spin on. No futexes, TLS,
// or signals: a waiting thread spins and yields.

#ifndef USCOPE_GOLDEN_THREAD_H
#define USCOPE_GOLDEN_THREAD_H

#include "rt.h"

typedef void (*rt_thread_fn)(void *);

// Starts a thread running fn(arg) on the stack [stack, stack + size). The
// thread exits with status 0 when fn returns. Returns the new thread's id.
long rt_spawn(rt_thread_fn fn, void *arg, char *stack, u64 size);

// Ends the calling thread alone.
_Noreturn void rt_exit(int code);

void rt_yield(void);

// Adds amount to *value atomically, returning the value before.
static inline u64 rt_add(u64 *value, u64 amount) {
    return __atomic_fetch_add(value, amount, __ATOMIC_SEQ_CST);
}

static inline u64 rt_load(const u64 *value) {
    return __atomic_load_n(value, __ATOMIC_SEQ_CST);
}

#endif
