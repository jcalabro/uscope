// Lines that make several calls, to step into one of them. Each function's
// first line is marked, as is each line of calls.
#include <stdio.h>
#include <string.h>

__attribute__((noinline)) int twice(int x) {
    return x * 2; // targets: twice
}

__attribute__((noinline)) int inc(int x) {
    return x + 1; // targets: inc
}

__attribute__((noinline)) int add(int a, int b) {
    return a + b; // targets: add
}

int (*volatile through)(int) = inc;

// Each activation calls itself before twice, so the step into twice runs
// the deeper ones through the same return address first.
__attribute__((noinline)) int fact(int n) {
    return n <= 1 ? 1 : n * fact(n - 1) + twice(n); // targets: fact
}

int main(int argc, char **argv) {
    int total = add(twice(argc), inc(argc)); // targets: calls
    total += through(total) + (int)strlen(argv[0]); // targets: indirect
    total += fact(argc + 2); // targets: recursive
    printf("%d\n", total); // targets: print
    return 0;
}
