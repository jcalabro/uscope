const std = @import("std");

pub var zig_global: i64 = 73;
pub var zig_sink: i64 = 0;
pub const zig_constant: i64 = 37;
var input_flag: bool = true;
var input_signed: i32 = -42;
var input_unsigned: u64 = 42;
var input_single: f32 = 1.25;
var input_double: f64 = -2.5;
var pointer_parameter_value: i32 = 42;

const SignedAlias = i32;
const UnsignedAlias = u64;
const PointerAlias = i32;
const PointerPair = struct {
    first: i32,
    second: i32,
};
const PointerNode = struct {
    next: ?*const @This(),
    value: i32,
};

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

noinline fn inspectPointers(value: i32, pointer_parameter: *const i32) bool {
    var pointee = value + 2;
    const pointer: *i32 = &pointee;
    const const_pointer: *const i32 = &pointee;
    const pointer_pointer: *const *i32 = &pointer;
    const many_pointer: [*]i32 = @ptrCast(pointer);
    const null_pointer: ?*i32 = null;
    const alias_pointee: PointerAlias = 42;
    const alias_pointer: *const PointerAlias = &alias_pointee;
    const pair = PointerPair{ .first = 20, .second = 22 };
    const structure_pointer: *const PointerPair = &pair;
    const node = PointerNode{ .next = null, .value = 42 };
    const recursive_pointer: *const PointerNode = &node;
    const array = [2]i32{ 20, 22 };
    const array_pointer: *const [2]i32 = &array;
    const slice: []const i32 = &array;
    std.mem.doNotOptimizeAway(&pointer);
    std.mem.doNotOptimizeAway(&const_pointer);
    std.mem.doNotOptimizeAway(&pointer_pointer);
    std.mem.doNotOptimizeAway(&many_pointer);
    std.mem.doNotOptimizeAway(&null_pointer);
    std.mem.doNotOptimizeAway(&pointer_parameter);
    std.mem.doNotOptimizeAway(&alias_pointer);
    std.mem.doNotOptimizeAway(&structure_pointer);
    std.mem.doNotOptimizeAway(&recursive_pointer);
    std.mem.doNotOptimizeAway(&array_pointer);
    std.mem.doNotOptimizeAway(&slice);
    zig_sink = pointer_pointer.*.*;
    return pointer.* == 42 and pointer_parameter.* == 42 and
        pair.first + pair.second == 42 and node.next == null and node.value == 42 and
        array[0] + array[1] == 42;
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
    const pointers = inspectPointers(40, &pointer_parameter_value);
    return @intFromBool(!succeeded or !pointers or scoped != -41 or zig_constant != 37);
}
