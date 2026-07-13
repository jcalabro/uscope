volatile int cpp_sink;

namespace fixture {

__attribute__((noinline)) bool inspect_scalars(bool flag, int signed_value,
                                               unsigned long unsigned_value,
                                               float single,
                                               double double_precision) {
    bool local_flag = !flag;
    int local_signed = signed_value + 1;
    unsigned long local_unsigned = unsigned_value + 2;
    float local_single = single + 0.5F;
    double local_double = double_precision - 0.25;
    cpp_sink = local_signed;
    return !local_flag && local_signed == -41 && local_unsigned == 44UL &&
           local_single == 1.75F && local_double == -2.75;
}

}  // namespace fixture

volatile bool input_flag = true;
volatile int input_signed = -42;
volatile unsigned long input_unsigned = 42UL;
volatile float input_single = 1.25F;
volatile double input_double = -2.5;

int main() {
    return fixture::inspect_scalars(input_flag, input_signed, input_unsigned,
                                    input_single, input_double)
               ? 0
               : 1;
}
