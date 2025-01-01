const std = @import("std");
const print = std.debug.print;

const max_depth = 5;

fn recursive(depth: *i32) void {
    if (depth.* > max_depth) {
        return;
    }

    print("recursion with depth: {}\n", .{depth.*});

    depth.* += 1;
    recursive(depth);
}

pub fn main() !void {
    print("first call:\n", .{});
    var depth1: i32 = 0;
    recursive(&depth1);

    print("\nsecond call:\n", .{});
    var depth2: i32 = 0;
    recursive(&depth2);
}
