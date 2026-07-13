const std = @import("std");

pub var zig_global: i64 = 73;
pub var zig_sink: i64 = 0;
pub const zig_constant: i64 = 37;
var input_flag: bool = true;
var input_signed: i32 = -42;
var input_unsigned: u64 = 42;
var input_single: f32 = 1.25;
var input_double: f64 = -2.5;

const SignedAlias = i32;
const UnsignedAlias = u64;

noinline fn inspectScalars(
    flag: bool,
    signed_value: SignedAlias,
    unsigned_value: UnsignedAlias,
    single: f32,
    double_precision: f64,
) bool {
    const local_flag = !flag;
    const local_signed: SignedAlias = signed_value + 1;
    const local_unsigned: UnsignedAlias = unsigned_value + 2;
    const local_single: f32 = single + 0.5;
    const local_double: f64 = double_precision - 0.25;
    zig_sink = local_signed;
    std.mem.doNotOptimizeAway(&local_flag);
    std.mem.doNotOptimizeAway(&local_unsigned);
    std.mem.doNotOptimizeAway(&local_single);
    std.mem.doNotOptimizeAway(&local_double);
    return !local_flag and local_signed == -41 and local_unsigned == 44 and
        local_single == 1.75 and local_double == -2.75;
}

noinline fn inspectScopes(value: i32) i32 {
    const outer_value = value + 1;
    {
        const nested_value = outer_value + 1;
        zig_sink = nested_value;
        std.mem.doNotOptimizeAway(&nested_value);
    }
    std.mem.doNotOptimizeAway(&outer_value);
    return outer_value;
}

pub fn main() u8 {
    const flag_ptr: *volatile bool = &input_flag;
    const signed_ptr: *volatile i32 = &input_signed;
    const unsigned_ptr: *volatile u64 = &input_unsigned;
    const single_ptr: *volatile f32 = &input_single;
    const double_ptr: *volatile f64 = &input_double;
    const succeeded = inspectScalars(
        flag_ptr.*,
        signed_ptr.*,
        unsigned_ptr.*,
        single_ptr.*,
        double_ptr.*,
    );
    const scoped = inspectScopes(signed_ptr.*);
    return @intFromBool(!succeeded or scoped != -41 or zig_constant != 37);
}
