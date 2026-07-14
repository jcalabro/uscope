volatile int spin_value = 42;
volatile int *spin_pointer = &spin_value;

__attribute__((noinline)) int unreached(void) {
    return 1;
}

int main(void) {
    for (;;) {
    }
}
