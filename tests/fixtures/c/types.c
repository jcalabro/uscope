typedef int base_alias;
typedef base_alias alias_chain;

struct node {
    const struct node *next;
    volatile alias_chain value;
};

__attribute__((noinline)) int inspect_types(int input) {
    int value = input;
    const int *const_pointee = &value;
    int *const const_pointer = &value;
    int *restrict restrict_pointer = &value;
    volatile alias_chain volatile_value = input;
    _Atomic int atomic_value = input;
    struct node recursive = {0, input};
    __asm__ volatile("" : : "g"(const_pointee), "g"(const_pointer),
                     "g"(restrict_pointer), "g"(&volatile_value), "g"(&atomic_value),
                     "g"(&recursive) : "memory");
    return *const_pointee + *const_pointer + *restrict_pointer +
           volatile_value + atomic_value + recursive.value;
}

int main(void) {
    return inspect_types(7) != 42;
}
