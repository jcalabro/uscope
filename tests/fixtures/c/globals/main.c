#include "globals.h"

volatile int32_t external_value = 101;
const int32_t external_constant = 303;
volatile int32_t global_sink, *external_pointer = &external_value;

__attribute__((noinline)) static int32_t inspect_globals(void) {
    int32_t external_value = 999;
    global_sink = external_value;
    return external_value;
}

int main(void) {
    int32_t local = inspect_globals();
    return local == 999 && file_one_value() == 201 &&
                   file_two_value() == 202 && external_constant == 303
               ? 0
               : 1;
}
