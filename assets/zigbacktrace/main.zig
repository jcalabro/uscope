const std = @import("std");

fn funcE() void {
    std.debug.print("funcE\n", .{});
}

fn funcD() void {
    funcE();
    std.debug.print("funcD\n", .{});
}

fn funcC() void {
    funcD();
    std.debug.print("funcC\n", .{});
}

fn funcB() void {
    funcC();
    std.debug.print("funcB\n", .{});
}

fn funcA() void {
    funcB();
    std.debug.print("funcA\n", .{});
}

pub fn main() !void {
    funcA();
    funcB();
    funcC();
    funcD();
    funcE();
}
