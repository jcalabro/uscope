// Loops, calls, recursion, and inlining in one thread. Runs as many rounds
// as its argument says, or three.

#include "../rt/rt.h"

u64 rounds_done;

static inline u64 square(u64 value) {
    return value * value;
}

__attribute__((noinline)) u64 sum_squares(u64 count) {
    u64 total = 0;
    for (u64 index = 1; index <= count; index++) {
        total += square(index); // MARK: total == (index - 1) * index * (2 * index - 1) / 6
    }
    return total; // MARK: total == count * (count + 1) * (2 * count + 1) / 6
}

__attribute__((noinline)) u64 fib(u64 index) {
    if (index < 2) {
        return index;
    }
    return fib(index - 1) + fib(index - 2);
}

int main(int argc, char **argv) {
    u64 rounds = argc > 1 ? rt_parse_u64(argv[1]) : 3;
    u64 total = 0;
    for (u64 round = 0; round < rounds; round++) {
        total += sum_squares(round + 4);
        total += fib(round + 5);
        rounds_done++;
    }
    rt_print("total ");
    rt_print_u64(total);
    rt_print("\n");
    return (int)(total % 100);
}
