#include <stdint.h>
#include <stdlib.h>

int32_t dso_apply(int32_t (*callback)(int32_t), int32_t value);

volatile int32_t frames_sink;

__attribute__((noinline)) static int compare_values(const void *left, const void *right) {
    int32_t a = *(const int32_t *)left;
    int32_t b = *(const int32_t *)right;
    frames_sink += 1;
    return (a > b) - (a < b);
}

__attribute__((noinline)) static void sort_values(void) {
    int32_t values[] = {3, 1, 2};
    qsort(values, sizeof(values) / sizeof(values[0]), sizeof(values[0]), compare_values);
    frames_sink += values[0];
}

__attribute__((noinline)) static int32_t module_callback(int32_t value) {
    frames_sink += value;
    return value * 2;
}

__attribute__((noinline)) static void abort_in_libc(void) {
    frames_sink += 1;
    abort();
}

int main(void) {
    sort_values();
    frames_sink += dso_apply(module_callback, 41);
    abort_in_libc();
}
