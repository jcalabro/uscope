// Processes for golden programs: fork, waiting for a child, and process
// identities. A waiting parent polls and yields rather than blocking.

#ifndef USCOPE_GOLDEN_PROCESS_H
#define USCOPE_GOLDEN_PROCESS_H

#include "rt.h"

// Forks the calling process. Returns the child's id in the parent and zero
// in the child, which has one thread: a copy of the caller.
long rt_fork(void);

// Waits for the child pid to exit, yielding while it runs, and returns its
// wait status.
int rt_wait(long pid);

// The calling process's id.
long rt_self(void);

// The parent process's id, which changes once the parent exits.
long rt_parent(void);

#endif
