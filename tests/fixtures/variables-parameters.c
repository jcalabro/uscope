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

volatile _Bool input_boolean = true;
volatile char input_character = 65;
volatile signed char input_signed_character = -12;
volatile unsigned char input_unsigned_character = 250;
volatile short input_signed_short = -1234;
volatile unsigned short input_unsigned_short = 54321;
volatile int input_signed_int = -1234567;
volatile unsigned int input_unsigned_int = 3456789012U;
volatile long input_signed_long = -123456789L;
volatile unsigned long input_unsigned_long = 123456789UL;
volatile long long input_signed_long_long = -1234567890123LL;
volatile unsigned long long input_unsigned_long_long = 12345678901234ULL;
volatile float input_single = 1.25F;
volatile double input_double_precision = -2.5;
volatile long double input_extended = 3.125L;

int main(void) {
    if (!all_parameters(
            input_boolean, input_character, input_signed_character,
            input_unsigned_character, input_signed_short, input_unsigned_short,
            input_signed_int, input_unsigned_int, input_signed_long,
            input_unsigned_long, input_signed_long_long,
            input_unsigned_long_long, input_single, input_double_precision,
            input_extended)) {
        return 1;
    }
    return shadow_parameter(100) == 100 && changing_parameter(10) == 24 ? 0 : 1;
}
