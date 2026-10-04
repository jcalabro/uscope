// Workers created with raw clone add their shares to a shared counter once
// every worker has started. The arguments say how many workers run, and how
// the program ends:
//
//   main    the main thread waits for the workers, which exit one by one,
//           then prints the total and exits the group
//   worker  the last worker to finish prints the total and exits the group
//           while the main thread waits, and others may still be exiting
//   leader  the main thread exits alone; the last worker to finish prints
//           the total, and the process ends when every worker has exited.
//           The thread that begins to exit last decides the process's
//           status, so every thread exits with the same status, 3, however
//           the threads interleave

#include "../rt/thread.h"

enum {
    MAX_WORKERS = 4,
    STACK_SIZE = 16384,
    ROUNDS = 5,
    LEADER_STATUS = 3,
};

enum ending { MAIN, WORKER, LEADER };

static char stacks[MAX_WORKERS][STACK_SIZE] __attribute__((aligned(16)));
static u64 workers;
static enum ending ending;
static u64 started;
static u64 finished;
u64 counter;

__attribute__((noinline)) u64 share(u64 index, u64 round) {
    return (index + 1) * 10 + round; // MARK: index < 4 && round < 5
}

static void report(void) {
    rt_print("total ");
    rt_print_u64(rt_load(&counter));
    rt_print("\n");
}

static void work(void *argument) {
    u64 index = (u64)argument;
    rt_add(&started, 1);
    while (rt_load(&started) < workers) {
        rt_yield();
    }
    for (u64 round = 0; round < ROUNDS; round++) {
        rt_add(&counter, share(index, round)); // MARK: index < 4 && round < 5
    }
    if (rt_add(&finished, 1) + 1 == workers && ending != MAIN) {
        report();
        if (ending == WORKER) {
            rt_exit_group((int)(rt_load(&counter) % 100));
        }
    }
    if (ending == LEADER) {
        rt_exit(LEADER_STATUS);
    }
}

int main(int argc, char **argv) {
    workers = argc > 1 ? rt_parse_u64(argv[1]) : 2;
    if (workers > MAX_WORKERS) {
        workers = MAX_WORKERS;
    }
    char mode = argc > 2 ? argv[2][0] : 'm';
    ending = mode == 'w' ? WORKER : mode == 'l' ? LEADER : MAIN;
    for (u64 index = 0; index < workers; index++) {
        rt_spawn(work, (void *)index, stacks[index], STACK_SIZE);
    }
    if (ending == LEADER) {
        rt_exit(LEADER_STATUS);
    }
    while (ending == WORKER || rt_load(&finished) < workers) {
        rt_yield();
    }
    report();
    return (int)(rt_load(&counter) % 100);
}
