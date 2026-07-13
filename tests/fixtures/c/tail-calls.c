#include <stdint.h>

volatile int tail_input = 3;
volatile int tail_sink;
volatile int tail_probe;
volatile int tail_trips = 200000;
volatile int tail_counter;

__attribute__((noinline)) int add_one(int value) {
    tail_sink = value;
    return value + 1;
}

__attribute__((noinline)) int chain_helper(int value) {
    tail_sink = value + 100;
    return add_one(value + 1);
}

static __attribute__((always_inline)) inline int inline_tail(int value) {
    int adjusted = value + 7;
    tail_sink = adjusted;
    return add_one(adjusted * 2);
}

__attribute__((noinline)) int outer_tail(int value) {
    return inline_tail(value);
}

static __attribute__((always_inline)) inline int inline_chain(int value) {
    int adjusted = value + 7;
    tail_sink = adjusted;
    return chain_helper(adjusted * 2);
}

__attribute__((noinline)) int outer_chain(int value) {
    return inline_chain(value);
}

__attribute__((noinline)) int mutual_tail(int value);

static __attribute__((always_inline)) inline int inline_descend(int value) {
    int adjusted = value + 1;
    tail_sink = adjusted;
    return mutual_tail(adjusted - 2);
}

__attribute__((noinline)) int descend_tail(int value) {
    return inline_descend(value);
}

__attribute__((noinline)) int mutual_tail(int value) {
    if (value <= 0) {
        return 1;
    }
    int inner = descend_tail(value);
    tail_probe = value;
    return inner + 1;
}

__attribute__((noinline)) int loop_helper(int trips) {
    uint32_t total = 0;
    for (int i = 0; i < trips; i++) {
        tail_counter += 1;
        total += (uint32_t)i | 1u;
    }
    return (int)(total & 0x7fffffffu);
}

static __attribute__((always_inline)) inline int inline_over_call(int value) {
    int scaled = loop_helper(tail_trips);
    tail_sink = scaled ^ value;
    return value + 1;
}

__attribute__((noinline)) int outer_over_call(int value) {
    int result = inline_over_call(value);
    return result;
}

int main(void) {
    int first = outer_tail(tail_input);
    int second = outer_chain(tail_input);
    int third = mutual_tail(2);
    int fourth = outer_over_call(tail_input);
    return first == 21 && second == 22 && third == 3 && fourth == 4 ? 0 : 1;
}
