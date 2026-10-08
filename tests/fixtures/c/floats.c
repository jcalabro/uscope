// Floats in every format x86-64 compilers give C: half, bfloat16, single,
// double, x87 extended, and quad precision. The quad value is a tenth, which
// shows its precision, since a tenth in any shorter format reads back as
// something else.

__attribute__((noinline)) void floats_target(void *values) {
    __asm__ volatile("" : : "r"(values) : "memory");
}

int main(void) {
    _Float16 half = (_Float16)1.5f;
    __bf16 brain = (__bf16)-3.140625f;
    float single = 0.25f;
    double precision = 2.5;
    long double extended = 3.125L;
    __float128 quad = (__float128)1 / 10;
    void *values[] = {&half, &brain, &single, &precision, &extended, &quad};
    floats_target(values);
    return 0;
}
