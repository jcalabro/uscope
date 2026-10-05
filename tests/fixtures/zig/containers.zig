//! Standard library containers, which the built-in views present. Each
//! `VIEW:` marker says what its expression must show, evaluated in main()
//! where barrier() is called; `problem:` says the view must refuse the
//! value, and why.

const std = @import("std");

noinline fn barrier(fixture: *const anyopaque) void {
    std.mem.doNotOptimizeAway(fixture);
}

pub fn main() !void {
    const allocator = std.heap.page_allocator;
    var ints: std.ArrayList(i32) = .empty; // VIEW: ints => len=3 [1, 2, 3]
    defer ints.deinit(allocator);
    try ints.appendSlice(allocator, &.{ 1, 2, 3 });
    var no_ints: std.ArrayList(u64) = .empty; // VIEW: no_ints => len=0 []
    var bytes = std.array_list.Managed(u8).init(allocator); // VIEW: bytes => len=2 [104, 105]
    defer bytes.deinit();
    try bytes.appendSlice("hi");
    var many: std.ArrayList(u32) = .empty; // VIEW: many => len=300 [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, …]
    defer many.deinit(allocator);
    for (0..300) |index| {
        try many.append(allocator, @intCast(index));
    }
    // A list whose length passes its capacity.
    var past_capacity: std.ArrayList(i32) = .empty; // VIEW: past_capacity => problem: check
    const storage = [_]i32{ 10, 11, 12, 13 };
    past_capacity.items = @constCast(storage[0..4]);
    past_capacity.capacity = 2;

    std.mem.doNotOptimizeAway(&ints);
    std.mem.doNotOptimizeAway(&no_ints);
    std.mem.doNotOptimizeAway(&bytes);
    std.mem.doNotOptimizeAway(&many);
    std.mem.doNotOptimizeAway(&past_capacity);
    barrier(&ints);
}
