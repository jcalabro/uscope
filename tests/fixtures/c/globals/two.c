#include "globals.h"

static volatile int32_t duplicate = 202;

__attribute__((noinline)) int32_t file_two_value(void) {
    return duplicate;
}
