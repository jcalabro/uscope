// Stores a watchpoint must report, and some it must not: stores to globals,
// stores of the value already there, rep stos and rep movs over a buffer,
// stores to neighbouring objects, and stores from several threads. Each
// round stores the bytes already in steady, pattern, and buffer before it
// changes them, and workers add zero to shared every other round. Runs as
// many rounds as its first argument says, or three, with as many workers
// as its second says, or two.

#include "../rt/thread.h"

enum {
    MAX_WORKERS = 3,
    STACK_SIZE = 16384,
    BUFFER_BYTES = 48,
};

static char stacks[MAX_WORKERS][STACK_SIZE] __attribute__((aligned(16)));
static u64 rounds;
static u64 workers;
static u64 finished;

u64 counter;
u64 steady = 7;
unsigned char buffer[BUFFER_BYTES];
unsigned char pattern[BUFFER_BYTES];
unsigned int neighbours[4];
u64 shared;

__attribute__((noinline)) void fill(unsigned char *bytes, unsigned char value, u64 count) {
    __asm__ volatile("rep stosb" : "+D"(bytes), "+c"(count) : "a"(value) : "memory");
}

__attribute__((noinline)) void copy(unsigned char *to, const unsigned char *from, u64 count) {
    __asm__ volatile("rep movsb" : "+D"(to), "+S"(from), "+c"(count) : : "memory");
}

__attribute__((noinline)) void bump(unsigned int *slot, unsigned int amount) {
    __atomic_fetch_add(slot, amount, __ATOMIC_SEQ_CST); // MARK: amount > 0 && amount < 8
}

static void work(void *argument) {
    u64 index = (u64)argument;
    for (u64 round = 0; round < rounds; round++) {
        rt_add(&shared, round % 2 == 0 ? index + 1 : 0);
        bump(&neighbours[(index + round) % 4], 1);
    }
    rt_add(&finished, 1);
}

int main(int argc, char **argv) {
    rounds = argc > 1 ? rt_parse_u64(argv[1]) : 3;
    workers = argc > 2 ? rt_parse_u64(argv[2]) : 2;
    if (workers > MAX_WORKERS) {
        workers = MAX_WORKERS;
    }
    for (u64 index = 0; index < workers; index++) {
        rt_spawn(work, (void *)index, stacks[index], STACK_SIZE);
    }
    u64 total = 0;
    for (u64 round = 0; round < rounds; round++) {
        counter += round + 1;
        steady = 7;
        fill(pattern, (unsigned char)round, BUFFER_BYTES);
        copy(buffer, pattern, BUFFER_BYTES);
        fill(pattern, (unsigned char)(round + 1), BUFFER_BYTES);
        copy(buffer, pattern, BUFFER_BYTES);
        bump(&neighbours[round % 4], 2);
        total += counter + steady + buffer[round % BUFFER_BYTES];
    }
    while (rt_load(&finished) < workers) {
        rt_yield();
    }
    for (u64 index = 0; index < 4; index++) {
        total += neighbours[index];
    }
    total += rt_load(&shared);
    rt_print("total ");
    rt_print_u64(total);
    rt_print("\n");
    return (int)(total % 100);
}
