__attribute__((noinline)) static int helper(int value) {
    return value + 5;
}

int shared_entry(int value) {
    return helper(value) - 2;
}
