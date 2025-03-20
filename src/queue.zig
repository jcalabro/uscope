const std = @import("std");
const Allocator = mem.Allocator;
const ArrayListUnmanaged = std.ArrayListUnmanaged;
const assert = std.debug.assert;
const Condition = Thread.Condition;
const heap = std.heap;
const mem = std.mem;
const Mutex = Thread.Mutex;
const time = std.time;
const Thread = std.Thread;
const t = std.testing;

/// A thread-safe Queue. The queue does not assume ownership over memory of its items, so
/// for instance if you're using a Queue([]const u8), be sure to free each slice.
pub fn Queue(comptime T: anytype) type {
    const Options = struct {
        /// The amount of time to wait on a dequeue operation before
        /// returning a Timeout error
        timeout_ns: u64 = time.ns_per_us * 10,
    };

    return struct {
        const Self = @This();

        /// The zero'th element in the list is the most recently
        /// inserted, the last element is the next one to be dequeued
        queue: ArrayListUnmanaged(T) = .empty,

        alloc: Allocator,
        opts: Options,

        mu: Mutex = .{},
        cond: Condition = .{},

        pub fn init(tsa: *heap.ThreadSafeAllocator, opts: Options) Self {
            return Self{
                .alloc = tsa.allocator(),
                .opts = opts,
            };
        }

        /// Callers should be sure to also free all the individual elements in the
        /// queue if required
        pub fn deinit(self: *Self) void {
            self.queue.deinit(self.alloc);
        }

        /// Clears the queue to zero length
        pub fn reset(self: *Self) void {
            self.mu.lock();
            defer self.mu.unlock();

            self.queue.clearAndFree(self.alloc);
        }

        /// Returns the number of items in the queue
        pub fn len(self: *Self) usize {
            self.mu.lock();
            defer self.mu.unlock();

            return self.queue.items.len;
        }

        /// Adds a single item to the back of the queue
        pub fn put(self: *Self, item: T) Allocator.Error!void {
            self.mu.lock();
            defer self.mu.unlock();

            // insert at the front
            try self.queue.insert(self.alloc, 0, item);
            self.cond.signal();
        }

        /// Dequeues an item, waiting until the configured timeout if no items
        /// are on the queue
        pub fn get(self: *Self) error{Timeout}!T {
            self.mu.lock();
            defer self.mu.unlock();

            while (self.queue.items.len == 0) {
                try self.cond.timedWait(&self.mu, self.opts.timeout_ns);
            }

            // dequeue from the back
            return self.queue.pop().?;
        }

        /// Dequeues an item, returning `null` immediately if no items are on the queue
        pub fn getOrNull(self: *Self) ?T {
            self.mu.lock();
            defer self.mu.unlock();

            return self.queue.pop();
        }
    };
}

test "queue:basic" {
    var tsa = heap.ThreadSafeAllocator{ .child_allocator = t.allocator };
    var q = Queue(u32).init(&tsa, .{});
    defer q.deinit();

    try t.expectEqual(0, q.len());
    try t.expectEqual(null, q.getOrNull());
    try t.expectError(error.Timeout, q.get());

    const a = 1;
    const b = 2;
    const c = 3;

    try q.put(a);
    try t.expectEqual(1, q.len());
    try t.expectEqual(a, q.getOrNull());
    try t.expectError(error.Timeout, q.get());
    try t.expectEqual(null, q.getOrNull());

    try q.put(a);
    try q.put(b);
    try q.put(c);

    try t.expectEqual(3, q.len());
    try t.expectEqual(a, q.getOrNull());
    try t.expectEqual(2, q.len());
    try t.expectEqual(b, try q.get());
    try t.expectEqual(1, q.len());
    try t.expectEqual(c, try q.get());
    try t.expectEqual(null, q.getOrNull());
    try t.expectError(error.Timeout, q.get());
}

test "queue:threads" {
    var tsa = heap.ThreadSafeAllocator{ .child_allocator = t.allocator };
    var queue = Queue(usize).init(&tsa, .{});
    defer queue.deinit();

    const S = struct {
        /// Adds and removes items from the shared queue across multiple threads
        /// which will trigger the race detector somewhat reliably if we messed up
        fn run(q: *Queue(usize)) void {
            const iterations = std.math.pow(usize, 2, 16);
            for (0..iterations) |ndx| {
                q.put(ndx) catch unreachable;
                _ = q.getOrNull();
                _ = q.len();
            }
        }
    };

    const t1 = try Thread.spawn(.{}, S.run, .{&queue});
    const t2 = try Thread.spawn(.{}, S.run, .{&queue});
    const t3 = try Thread.spawn(.{}, S.run, .{&queue});
    const t4 = try Thread.spawn(.{}, S.run, .{&queue});

    t1.join();
    t2.join();
    t3.join();
    t4.join();
}
