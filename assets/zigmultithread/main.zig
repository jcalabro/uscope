const std = @import("std");
const print = std.debug.print;
const Thread = std.Thread;
const WaitGroup = Thread.WaitGroup;

pub fn main() !void {
    print("starting zigmultithread\n", .{});
    defer print("zigmultithread done\n", .{});

    var gpa = std.heap.GeneralPurposeAllocator(.{}){};
    const alloc = gpa.allocator();
    const wg = try alloc.create(WaitGroup);
    wg.* = .{};

    {
        wg.start();
        const thread = try Thread.spawn(.{}, sleepThread, .{ wg, 1 });
        try thread.setName("thread1");
    }

    {
        wg.start();
        const thread = try Thread.spawn(.{}, sleepThread, .{ wg, 2 });
        try thread.setName("thread2");
    }

    {
        wg.start();
        const thread = try Thread.spawn(.{}, sleepThread, .{ wg, 3 });
        try thread.setName("thread3");
    }

    wg.wait();
}

fn sleepThread(wg: *WaitGroup, sleep_secs: u64) void {
    defer wg.finish();

    print("sleeping for {d}s\n", .{sleep_secs});
    Thread.sleep(sleep_secs * std.time.ns_per_s);
    print("sleep for {d}s complete\n", .{sleep_secs});
}
