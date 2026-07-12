#include <stdbool.h>

volatile long long parameter_sink;

__attribute__((noinline)) static int all_parameters(
    _Bool boolean,
    char character,
    signed char signed_character,
    unsigned char unsigned_character,
    short signed_short,
    unsigned short unsigned_short,
    int signed_int,
    unsigned int unsigned_int,
    long signed_long,
    unsigned long unsigned_long,
    long long signed_long_long,
    unsigned long long unsigned_long_long,
    float single,
    double double_precision,
    long double extended) {
    int local = 99;
    parameter_sink = local;
    return boolean && character == 65 && signed_character == -12 &&
                   unsigned_character == 250 && signed_short == -1234 &&
                   unsigned_short == 54321 && signed_int == -1234567 &&
                   unsigned_int == 3456789012U && signed_long == -123456789L &&
                   unsigned_long == 123456789UL &&
                   signed_long_long == -1234567890123LL &&
                   unsigned_long_long == 12345678901234ULL && single == 1.25F &&
                   double_precision == -2.5 && extended == 3.125L && local == 99;
}

__attribute__((noinline)) static int shadow_parameter(int shadowed) {
    {
        int shadowed = 200;
        parameter_sink = shadowed;
    }
    return shadowed;
}

__attribute__((noinline)) static int changing_parameter(int changing) {
    for (int iteration = 0; iteration < 2; ++iteration) {
        changing += 7;
        parameter_sink = changing;
    }
    return changing;
}

int main(void) {
    if (!all_parameters(true, 65, -12, 250, -1234, 54321, -1234567,
                        3456789012U, -123456789L, 123456789UL,
                        -1234567890123LL, 12345678901234ULL, 1.25F, -2.5,
                        3.125L)) {
        return 1;
    }
    return shadow_parameter(100) == 100 && changing_parameter(10) == 24 ? 0 : 1;
}
