const std = @import("std");
const Allocator = mem.Allocator;
const ArrayList = std.ArrayList;
const assert = std.debug.assert;
const Condition = Thread.Condition;
const mem = std.mem;
const Mutex = Thread.Mutex;
const time = std.time;
const Thread = std.Thread;

pub fn Queue(comptime T: anytype) type {
    const Options = struct {
        /// the amount of time to wait on a dequeue operation before
        /// returning a Timeout error
        timeout_ns: u64 = time.ns_per_us * 10,
    };

    return struct {
        const Self = @This();

        // the zero'th element in the list is
        // the most recently inserted, the last
        // element is the next one to be dequeued
        queue: ArrayList(T),

        allocator: Allocator,
        opts: Options,

        mu: Mutex = .{},
        cond: Condition = .{},

        /// Must be a ThreadSafeAllocator
        pub fn init(thread_safe_alloc: Allocator, opts: Options) Self {
            return Self{
                .allocator = thread_safe_alloc,
                .opts = opts,
                .queue = ArrayList(T).init(thread_safe_alloc),
            };
        }

        /// Callers should be sure to also free all the individual elements in the
        /// queue if required
        pub fn deinit(self: *Self) void {
            self.queue.deinit();
        }

        pub fn reset(self: *Self) void {
            self.mu.lock();
            defer self.mu.unlock();

            self.queue.clearAndFree();
        }

        pub fn len(self: *Self) usize {
            self.mu.lock();
            defer self.mu.unlock();

            return self.queue.items.len;
        }

        pub fn put(self: *Self, item: T) Allocator.Error!void {
            self.mu.lock();
            defer self.mu.unlock();

            // insert at the front
            try self.queue.insert(0, item);
            self.cond.signal();
        }

        pub fn get(self: *Self) error{Timeout}!T {
            self.mu.lock();
            defer self.mu.unlock();

            while (self.queue.items.len == 0) {
                try self.cond.timedWait(&self.mu, self.opts.timeout_ns);
            }

            // dequeue from the back
            return self.queue.pop();
        }

        pub fn getOrNull(self: *Self) ?T {
            self.mu.lock();
            defer self.mu.unlock();

            if (self.queue.items.len == 0) return null;
            return self.queue.pop();
        }
    };
}
