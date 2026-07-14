#include <stdbool.h>

volatile int pointer_sink;
int pointer_parameter_value = 42;

typedef int aliased_int;

struct pointer_pair {
    int first;
    int second;
};

struct pointer_node {
    struct pointer_node *next;
    int value;
};

__attribute__((noinline)) static int pointer_identity(int value) {
    return value;
}

__attribute__((noinline)) static int parameter_target(int parameter) {
    int local = parameter + 1;
    return local;
}

__attribute__((noinline)) static int changing_target(void) {
    int changing = 10;
    for (int iteration = 0; iteration < 2; ++iteration) {
        changing += 7;
    }
    return changing;
}

__attribute__((noinline)) static int shadow_target(void) {
    int shadowed = 100;
    {
        int shadowed = 200;
        shadowed += 1;
        (void)shadowed;
    }
    return shadowed;
}

__attribute__((noinline)) static int partial_target(void) {
    int available = 42;
    int *pointer = &available;
    return *pointer;
}

__attribute__((noinline)) int pointer_target(int parameter, int *pointer_parameter) {
    int pointee = parameter + 2;
    int *pointer = &pointee;
    int **pointer_pointer = &pointer;
    const int *const_pointee = &pointee;
    int *const const_pointer = &pointee;
    void *void_pointer = &pointee;
    int *null_pointer = 0, *invalid_pointer = (int *)1;
    aliased_int alias_pointee = 42;
    aliased_int *alias_pointer = &alias_pointee;
    struct pointer_pair pair = {20, 22};
    struct pointer_pair *structure_pointer = &pair;
    struct pointer_node node = {0, 42};
    struct pointer_node *recursive_pointer = &node;
    int array[2] = {20, 22};
    int (*array_pointer)[2] = &array;
    int (*function_pointer)(int) = pointer_identity;
    __asm__ volatile("" : : "g"(pointer), "g"(pointer_pointer), "g"(const_pointee),
                     "g"(const_pointer), "g"(void_pointer), "g"(null_pointer),
                     "g"(invalid_pointer), "g"(pointer_parameter), "g"(alias_pointer),
                     "g"(structure_pointer), "g"(recursive_pointer), "g"(array_pointer),
                     "g"(function_pointer) : "memory");
    return **pointer_pointer + *pointer_parameter - 42;
}

__attribute__((noinline)) static int implicit_pointer_target(int parameter) {
    volatile int pointee = parameter + 2;
    volatile int *pointer = &pointee;
    volatile int **pointer_pointer = &pointer;
    pointer_sink = **pointer_pointer;
    return pointer_sink;
}

__attribute__((noinline)) static int implicit_pointer_offset_target(int parameter) {
    unsigned long storage = (unsigned long)(parameter + 2) << 32;
    unsigned char *byte_pointer = (unsigned char *)&storage + 4;
    pointer_sink = *byte_pointer;
    return pointer_sink;
}

int main(void) {
    _Bool boolean = true;
    char character = 65;
    signed char signed_character = -12;
    unsigned char unsigned_character = 250;
    short signed_short = -1234;
    unsigned short unsigned_short = 54321;
    int signed_int = -1234567;
    unsigned int unsigned_int = 3456789012U;
    long signed_long = -123456789L;
    unsigned long unsigned_long = 123456789UL;
    long long signed_long_long = -1234567890123LL;
    unsigned long long unsigned_long_long = 12345678901234ULL;
    float single = 1.25F;
    double double_precision = -2.5;
    long double extended = 3.125L;

    if (parameter_target(4) + changing_target() + shadow_target() + partial_target() +
            pointer_target(40, &pointer_parameter_value) +
            pointer_target(40, &pointer_parameter_value) + implicit_pointer_target(40) +
            implicit_pointer_offset_target(40) != 339) {
        return 1;
    }
    return boolean && character == 65 && signed_character == -12 &&
                   unsigned_character == 250 && signed_short == -1234 &&
                   unsigned_short == 54321 && signed_int == -1234567 &&
                   unsigned_int == 3456789012U && signed_long == -123456789L &&
                   unsigned_long == 123456789UL &&
                   signed_long_long == -1234567890123LL &&
                   unsigned_long_long == 12345678901234ULL && single == 1.25F &&
                   double_precision == -2.5 && extended == 3.125L
               ? 0
               : 1;
}
