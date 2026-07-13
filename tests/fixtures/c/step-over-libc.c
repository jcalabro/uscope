#include <unistd.h>

volatile long step_sink;

__attribute__((noinline)) static void call_libc(void) {
    step_sink = (long)getpid();
    step_sink += 1;
}

int main(void) {
    call_libc();
    return step_sink > 0 ? 0 : 1;
}
