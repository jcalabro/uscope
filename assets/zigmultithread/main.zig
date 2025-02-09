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
        const thread = try Thread.spawn(.{}, sleepThread, .{ wg, 100 });
        try thread.setName("thread100");
    }

    {
        wg.start();
        const thread = try Thread.spawn(.{}, sleepThread, .{ wg, 200 });
        try thread.setName("thread200");
    }

    wg.wait();
}

fn sleepThread(wg: *WaitGroup, sleep_ms: u64) void {
    defer wg.finish();

    print("sleeping for {d}ms\n", .{sleep_ms});
    Thread.sleep(sleep_ms * std.time.ns_per_ms);
    print("sleep for {d}ms complete\n", .{sleep_ms});
}
