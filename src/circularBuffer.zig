const std = @import("std");
const Allocator = std.mem.Allocator;
const t = std.testing;

pub fn CircularBuffer(comptime T: type) type {
    return struct {
        const Self = @This();

        /// Not intended to be accessed by callers directly
        items: []T,

        read_ndx: usize = 0,
        write_ndx: usize = 0,
        len: usize = 0,

        pub fn init(alloc: Allocator, size: usize) !Self {
            return Self{ .items = try alloc.alloc(T, size) };
        }

        pub fn deinit(self: *Self, alloc: Allocator) void {
            alloc.free(self.items);
        }

        pub fn clearAndReset(self: *Self, alloc: Allocator) !void {
            self.read_ndx = 0;
            self.write_ndx = 0;
            self.len = 0;

            const size = self.items.len;
            alloc.free(self.items);
            self.items = try alloc.alloc(T, size);
        }

        pub fn append(self: *Self, item: T) void {
            self.items[self.write_ndx] = item;

            if (self.len > 0 and self.write_ndx <= self.read_ndx) {
                self.read_ndx += 1;
            }

            self.len += 1;
            if (self.len > self.items.len) {
                self.len = self.items.len;
            }

            self.write_ndx += 1;
            if (self.write_ndx >= self.items.len) {
                self.write_ndx = 0;
            }
        }

        pub fn get(self: Self, ndx: usize) T {
            const i = (self.read_ndx + ndx) % self.items.len;
            return self.items[i];
        }
    };
}

test "CircularBuffer" {
    const size = 4;

    var buf = try CircularBuffer(usize).init(t.allocator, size);
    defer buf.deinit(t.allocator);

    try t.expectEqual(0, buf.read_ndx);
    try t.expectEqual(0, buf.write_ndx);
    try t.expectEqual(0, buf.len);
    try t.expectEqual(size, buf.items.len);

    // fill the buffer
    for (0..size) |ndx| {
        buf.append(ndx);
        try t.expectEqual(ndx + 1, buf.len);
    }

    // check the buffer contents
    for (0..buf.len) |ndx| {
        try t.expectEqual(ndx, buf.get(ndx));
    }

    // append some more and check again
    buf.append(size);
    try t.expectEqual(1, buf.read_ndx);
    try t.expectEqual(1, buf.write_ndx);
    try t.expectEqual(size, buf.len);
    for (0..buf.len) |ndx| {
        try t.expectEqual(ndx + 1, buf.get(ndx));
    }

    buf.append(size + 1);
    try t.expectEqual(2, buf.read_ndx);
    try t.expectEqual(2, buf.write_ndx);
    try t.expectEqual(size, buf.len);
    for (0..buf.len) |ndx| {
        try t.expectEqual(ndx + 2, buf.get(ndx));
    }
}
