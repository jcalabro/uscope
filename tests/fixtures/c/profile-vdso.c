// Time a profiler test samples: main reads the clock in a loop through
// the vDSO, the kernel's shared object that no file holds, for the number
// of reads its argument names.
#define _POSIX_C_SOURCE 199309L
#include <stdlib.h>
#include <time.h>

volatile long sink;

__attribute__((noinline)) void read_clock(long reads) {
    struct timespec now;
    for (long read = 0; read < reads; ++read) {
        clock_gettime(CLOCK_MONOTONIC, &now);
        sink += now.tv_nsec;
    }
}

int main(int argc, char **argv) {
    read_clock(argc > 1 ? atol(argv[1]) : 1);
    return 0;
}
