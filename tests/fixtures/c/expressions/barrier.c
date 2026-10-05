// The expressions test stops here and evaluates its caller's expressions.
// Taking the fixture's address makes its storage live in memory at every
// optimization level.
__attribute__((noinline)) void barrier(void *fixture) {
    __asm__ volatile("" : : "r"(fixture) : "memory");
}
