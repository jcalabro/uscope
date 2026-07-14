const std = @import("std");

pub var root_value: i64 = 171;
pub const root_constant: i64 = 172;
pub var pointer_value: i32 = 184;
pub var root_pointer: *const i32 = &pointer_value;
var global_sink: i64 = 0;

const Alpha = struct {
    pub var duplicate: i32 = 181;
    pub const constant: i32 = 182;
};

const Beta = struct {
    pub var duplicate: i32 = 183;
};

noinline fn inspectGlobals() void {
    global_sink = root_value + root_constant + Alpha.duplicate +
        Alpha.constant + Beta.duplicate + root_pointer.*;
    std.mem.doNotOptimizeAway(&global_sink);
}

pub fn main() u8 {
    inspectGlobals();
    return @intFromBool(global_sink != 1073);
}
