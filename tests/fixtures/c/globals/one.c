#include "globals.h"

static volatile int32_t duplicate = 201;

__attribute__((noinline)) int32_t file_one_value(void) {
    return duplicate;
}
