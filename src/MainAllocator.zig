//! An allocator that changes based on the optimization mode
const std = @import("std");
const builtin = @import("builtin");
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;
const heap = std.heap;

const Self = @This();

gpa: heap.GeneralPurposeAllocator(.{}),
alloc: Allocator,

pub inline fn init() Self {
    var gpa = heap.GeneralPurposeAllocator(.{}){};
    return Self{
        .gpa = gpa,
        .alloc = switch (builtin.mode) {
            .Debug => gpa.allocator(),
            else => heap.c_allocator,
        },
    };
}

pub fn allocator(self: Self) Allocator {
    return self.alloc;
}

pub fn deinit(self: *Self) void {
    defer assert(self.gpa.deinit() == .ok);
}
