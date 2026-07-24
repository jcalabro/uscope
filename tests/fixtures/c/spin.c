volatile int spin_value = 42;
volatile int *spin_pointer = &spin_value;
volatile int spin_values[4] = {40, 41, 42, 43};

__attribute__((noinline)) int unreached(void) {
    return 1;
}

int main(void) {
    for (;;) {
    }
}
