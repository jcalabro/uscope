volatile int inline_input = 7;
volatile int inline_sink;

static __attribute__((always_inline)) inline int inline_target(int value) {
    int inline_local = value + 3;
    inline_sink = inline_local *= 2;
    return inline_local;
}

__attribute__((noinline)) static int inline_caller(int value) {
    int caller_local = value + 1;
    int inline_result = inline_target(caller_local);
    return inline_result + caller_local;
}

int main(void) {
    return inline_caller(inline_input) == 30 ? 0 : 1;
}
