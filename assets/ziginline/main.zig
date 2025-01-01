const std = @import("std");
const print = std.debug.print;

fn inlinedFunc() i32 {
    print("inlinedFunc called 1\n", .{});
    print("inlinedFunc called 2\n", .{});
    return 11;
}

fn notInlinedFunc() usize {
    print("notInlinedFunc called 1\n", .{});
    print("notInlinedFunc called 2\n", .{});
    return 22;
}

pub fn main() !void {
    const a = @call(.never_inline, notInlinedFunc, .{});
    const b = @call(.always_inline, inlinedFunc, .{});
    const c = @call(.never_inline, notInlinedFunc, .{});

    print("a: {}\n", .{a});
    print("b: {}\n", .{b});
    print("c: {}\n", .{c});
}
