//! An allocator that changes based on the optimization mode
const std = @import("std");
const builtin = @import("builtin");
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;
const heap = std.heap;

const MainAllocator = @This();

gpa: heap.GeneralPurposeAllocator(.{}),
alloc: Allocator,

pub inline fn init() MainAllocator {
    var gpa = heap.GeneralPurposeAllocator(.{}){};
    return .{
        .gpa = gpa,
        .alloc = switch (builtin.mode) {
            .Debug => gpa.allocator(),
            else => heap.c_allocator,
        },
    };
}

pub fn allocator(self: MainAllocator) Allocator {
    return self.alloc;
}

pub fn deinit(self: *MainAllocator) void {
    defer assert(self.gpa.deinit() == .ok);
}
