const std = @import("std");

pub var watched: u64 = 0;

noinline fn watchReady() void {
    @as(*volatile u64, &watched).* = 1;
}

noinline fn watchedWrites() void {
    for ([_]u64{ 2, 3, 3 }) |value| {
        @as(*volatile u64, &watched).* = value;
    }
}

pub fn main() u8 {
    watchReady();
    watchedWrites();
    return @intFromBool(@as(*volatile u64, &watched).* != 3);
}
