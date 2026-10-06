// Values optimized code assembles from pieces. Before
// each checkpoint the program prints its own truth, one tab-separated line
// per value,
//
//	TRUTH	<checkpoint>	<path>	<kind>	<value>
//
// and then calls reached(checkpoint); the tests inspect reached's caller.
// A path names a variable, or a member of one after a dot.
#include <stdio.h>

struct pair {
    long first;
    long second;
};

volatile long sink;

__attribute__((noinline)) void reached(const char *checkpoint) {
    sink = (long)checkpoint;
    __asm__ volatile("" ::: "memory");
}

// The pair arrives in two registers, and its sum stays in one across the
// call, so optimized code describes both in pieces.
__attribute__((noinline)) static long split(struct pair pair, long bias) {
    struct pair local = pair;
    local.first += bias;
    printf("TRUTH\tsplit\tpair.first\tint\t%ld\n", pair.first);
    printf("TRUTH\tsplit\tpair.second\tint\t%ld\n", pair.second);
    printf("TRUTH\tsplit\tlocal.first\tint\t%ld\n", local.first);
    printf("TRUTH\tsplit\tlocal.second\tint\t%ld\n", local.second);
    fflush(stdout);
    reached("split");
    sink = local.second;
    return local.first;
}

int main(int argc, char **argv) {
    (void)argv;
    struct pair pair = {argc * 10L, argc * 20L};
    long result = split(pair, argc);
    return result == 11 ? 0 : 1;
}
