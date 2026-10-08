//! Zig's half and quad precision floats. The quad value is a tenth, which
//! shows its precision, since a tenth in any shorter format reads back as
//! something else.

const std = @import("std");

noinline fn floatsTarget(half: f16, quad: f128) void {
    std.mem.doNotOptimizeAway(half);
    std.mem.doNotOptimizeAway(quad);
}

pub fn main() void {
    var one: f128 = 1;
    std.mem.doNotOptimizeAway(&one);
    floatsTarget(1.5, one / 10);
}
