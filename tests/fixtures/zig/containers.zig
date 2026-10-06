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
    var hashed = std.AutoHashMap(i32, i32).init(allocator); // VIEW: hashed => len=2 {1: 10, 2: 20} (any order)
    defer hashed.deinit();
    try hashed.put(1, 10);
    try hashed.put(2, 20);
    var unmanaged: std.AutoHashMapUnmanaged(u32, u64) = .empty; // VIEW: unmanaged => len=1 {3: 30}
    defer unmanaged.deinit(allocator);
    try unmanaged.put(allocator, 3, 30);
    var no_hashed: std.AutoHashMapUnmanaged(u32, u64) = .empty; // VIEW: no_hashed => len=0 {}
    var named = std.StringHashMap(i32).init(allocator); // VIEW: named => len=1 {"one": 1}
    defer named.deinit();
    try named.put("one", 1);
    var many_hashed = std.AutoHashMap(u32, u32).init(allocator); // VIEW: many_hashed => count: 300
    defer many_hashed.deinit();
    for (0..300) |index| {
        try many_hashed.put(@intCast(index), @intCast(index * 2));
    }
    var ordered: std.AutoArrayHashMapUnmanaged(i32, i32) = .empty; // VIEW: ordered => len=2 {5: 50, 6: 60}
    defer ordered.deinit(allocator);
    try ordered.put(allocator, 5, 50);
    try ordered.put(allocator, 6, 60);
    var strings: std.StringArrayHashMapUnmanaged(i32) = .empty; // VIEW: strings => len=1 {"k": 1}
    defer strings.deinit(allocator);
    try strings.put(allocator, "k", 1);
    var no_ordered: std.AutoArrayHashMapUnmanaged(i32, i32) = .empty; // VIEW: no_ordered => len=0 {}
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
    std.mem.doNotOptimizeAway(&hashed);
    std.mem.doNotOptimizeAway(&unmanaged);
    std.mem.doNotOptimizeAway(&no_hashed);
    std.mem.doNotOptimizeAway(&named);
    std.mem.doNotOptimizeAway(&many_hashed);
    std.mem.doNotOptimizeAway(&ordered);
    std.mem.doNotOptimizeAway(&strings);
    std.mem.doNotOptimizeAway(&no_ordered);
    barrier(&ints);
}
