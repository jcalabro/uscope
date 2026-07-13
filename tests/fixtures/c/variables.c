#include <stdbool.h>

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
    int *unsupported_pointer = &available;
    return *unsupported_pointer;
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

    if (parameter_target(4) + changing_target() + shadow_target() + partial_target() != 171) {
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
