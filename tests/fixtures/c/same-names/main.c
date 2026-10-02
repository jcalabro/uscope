// Two files define a static function with the same name, and this file
// calls a function another file defines, which GCC describes here by a
// declaration.
int shared_entry(int value);

__attribute__((noinline)) static int helper(int value) {
    return value * 2;
}

int main(void) {
    return helper(3) + shared_entry(4) == 13 ? 0 : 1;
}
