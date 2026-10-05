// Forks children the debugger must release clean: children the parent
// waits for, a child that outlives its parent, and a child forked from a
// worker thread. Every child redoes its parent's work in its own copy of
// memory and exits 0 when that agrees, so a child released with a trap in
// place, which dies of SIGTRAP, shows. Runs as many rounds of waited
// children as its first argument says, or three, in the mode its second
// names, or 2: 0 forks only those; 1 also leaves an orphan; 2 also forks
// from a worker thread.

#include "../rt/process.h"
#include "../rt/thread.h"

enum {
    STACK_SIZE = 16384,
};

static char stack[STACK_SIZE] __attribute__((aligned(16)));
static u64 worker_done;
static u64 worker_ok;

// The parent's count of children; a child's writes stay in its own copy.
u64 forks;
// What the parent last asked a child to check.
u64 asked;

__attribute__((noinline)) u64 work(u64 n) {
    u64 sum = 0;
    for (u64 i = 1; i <= n; i++) {
        sum += i * i; // MARK: i >= 1 && i <= n
    }
    return sum;
}

// Runs in a child: redoes the parent's work, and exits 0 when its copy of
// memory agrees with the parent's as it forked.
__attribute__((noinline, noreturn)) void child(u64 n, u64 expected) {
    u64 got = work(n);
    forks += 100;
    rt_exit_group(got == expected && asked == n ? 0 : 1);
}

// Forks a child to check work(n) and waits for it. Returns 1 when it
// exited 0.
static u64 fork_and_wait(u64 n) {
    u64 expected = work(n);
    asked = n;
    long pid = rt_fork();
    if (pid == 0) {
        child(n, expected);
    }
    rt_add(&forks, 1);
    int status = rt_wait(pid);
    return status == 0;
}

// Forks a child that waits for this process to exit before it checks
// work(n), so it is released after its parent is gone.
static void leave_orphan(u64 n) {
    u64 expected = work(n);
    asked = n;
    long self = rt_self();
    long pid = rt_fork();
    if (pid == 0) {
        while (rt_parent() == self) {
            rt_yield();
        }
        child(n, expected);
    }
}

static void worker(void *argument) {
    worker_ok = fork_and_wait((u64)argument);
    rt_add(&worker_done, 1);
}

int main(int argc, char **argv) {
    u64 rounds = argc > 1 ? rt_parse_u64(argv[1]) : 3;
    u64 mode = argc > 2 ? rt_parse_u64(argv[2]) : 2;
    u64 ok = 0;
    for (u64 round = 1; round <= rounds; round++) {
        ok += fork_and_wait(round);
    }
    if (mode >= 2) {
        rt_spawn(worker, (void *)(rounds + 1), stack, STACK_SIZE);
        while (rt_load(&worker_done) == 0) {
            rt_yield();
        }
        ok += worker_ok;
    }
    // No child's write reached this copy.
    ok += rt_load(&forks) == rounds + (mode >= 2);
    if (mode >= 1) {
        leave_orphan(rounds + 2);
    }
    rt_print("ok ");
    rt_print_u64(ok);
    rt_print("\n");
    return (int)ok;
}
