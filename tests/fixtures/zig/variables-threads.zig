const std = @import("std");

var workers_ready: std.atomic.Value(u8) = .init(0);
var release_workers: std.atomic.Value(bool) = .init(false);
var thread_sum: std.atomic.Value(u16) = .init(0);

noinline fn workerBreakpoint(value: u16) void {
    _ = thread_sum.fetchAdd(value, .monotonic);
}

fn worker(value: u16) void {
    const thread_value = value;
    _ = workers_ready.fetchAdd(1, .release);
    while (!release_workers.load(.acquire)) {
        std.Thread.yield() catch {};
    }
    workerBreakpoint(thread_value);
}

pub fn main() !u8 {
    const second = try std.Thread.spawn(.{}, worker, .{@as(u16, 202)});
    const first = try std.Thread.spawn(.{}, worker, .{@as(u16, 101)});
    while (workers_ready.load(.acquire) != 2) {
        std.Thread.yield() catch {};
    }
    release_workers.store(true, .release);
    first.join();
    second.join();
    return @intFromBool(thread_sum.load(.acquire) != 303);
}
