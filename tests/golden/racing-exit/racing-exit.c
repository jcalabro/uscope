// Threads that keep calling tick() until one of them exits the whole group:
// worker 0 does once it has ticked as many times as the second argument
// says. Every other thread, the main thread among them, ticks until the
// group exit ends it, so the exit lands wherever the others happen to be.

#include "../rt/thread.h"

enum {
    MAX_WORKERS = 4,
    STACK_SIZE = 16384,
};

static char stacks[MAX_WORKERS][STACK_SIZE] __attribute__((aligned(16)));
static u64 rounds;
// One count per worker, and the main thread's last.
u64 ticks[MAX_WORKERS + 1];

__attribute__((noinline)) void tick(u64 *count) {
    rt_add(count, 1);
}

static void spin(void *argument) {
    u64 index = (u64)argument;
    for (;;) {
        tick(&ticks[index]);
        if (index == 0 && rt_load(&ticks[0]) == rounds) {
            rt_print("exit after ");
            rt_print_u64(rounds);
            rt_print("\n");
            rt_exit_group((int)rounds);
        }
        rt_yield();
    }
}

int main(int argc, char **argv) {
    u64 workers = argc > 1 ? rt_parse_u64(argv[1]) : 2;
    if (workers < 1) {
        workers = 1;
    }
    if (workers > MAX_WORKERS) {
        workers = MAX_WORKERS;
    }
    rounds = argc > 2 ? rt_parse_u64(argv[2]) : 3;
    for (u64 index = 0; index < workers; index++) {
        rt_spawn(spin, (void *)index, stacks[index], STACK_SIZE);
    }
    for (;;) {
        tick(&ticks[MAX_WORKERS]);
        rt_yield();
    }
}
