#include <stdint.h>

volatile int inline_input = 3;
volatile int inline_sink;

static __attribute__((always_inline)) inline int leaf(int value) {
    int incremented = value + 1;

    inline_sink = incremented;
    return incremented * 2;
}

static __attribute__((always_inline)) inline int middle(int value) {
    int nested = leaf(value);

    return nested + 3;
}

static __attribute__((always_inline)) inline int branchy(int value) {
    if (value < 0) {
        return leaf(-value);
    }

    return leaf(value) + 5;
}

__attribute__((noinline)) int caller(int value) {
    int first = middle(value);
    int second = middle(first);
    int same_line = leaf(second) + leaf(second + 1);

    return branchy(same_line);
}

int main(void) {
    return caller(inline_input) == 235 ? 0 : 1;
}
