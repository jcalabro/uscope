const std = @import("std");
const builtin = @import("builtin");
const ArenaAllocator = heap.ArenaAllocator;
const ArrayList = std.ArrayList;
const assert = std.debug.assert;
const fs = std.fs;
const heap = std.heap;
const Thread = std.Thread;
const ThreadSafeAllocator = heap.ThreadSafeAllocator;

const logging = @import("logging.zig");

const Debugger = @import("debugger.zig").Debugger;
const file = @import("file.zig");
const flags = @import("flags.zig");
const gui = @import("gui.zig");
const MainAllocator = @import("MainAllocator.zig");
const settings = @import("settings.zig");
const trace = @import("trace.zig");

const log = logging.Logger.init(logging.Region.Main);

pub fn main() !void {
    var main_allocator = MainAllocator.init();
    defer main_allocator.deinit();

    var thread_safe_allocator = ThreadSafeAllocator{ .child_allocator = main_allocator.allocator() };
    const thread_safe_alloc = thread_safe_allocator.allocator();

    trace.init();
    defer trace.deinit();

    trace.initThread();
    defer trace.deinitThread();

    const z = trace.zone(@src());
    defer z.end();

    var settings_arena = ArenaAllocator.init(main_allocator.allocator());
    defer settings_arena.deinit();
    try settings.parseFiles(settings_arena.allocator());

    if (settings.settings.project.target.path.len == 0) {
        std.debug.print("target.path is required in the project settings file, but was not provided\n", .{});
        std.process.exit(1);
    }

    try logging.init(.{
        .allocator = thread_safe_alloc,
        .level = settings.settings.global.log.level,
        .regions = settings.settings.global.log.regions,
        .color = settings.settings.global.log.color,
        .fp = try openOrCreateLogFile(settings.settings.global.log.file),
    });
    defer logging.deinit();

    log.info("inspect starting");
    defer log.info("inspect done");

    flags.logAll();

    var file_arena = ArenaAllocator.init(main_allocator.allocator());
    defer file_arena.deinit();
    file.initHashCache(file_arena.allocator());

    var dbg = try Debugger.init(&thread_safe_allocator);
    defer dbg.deinit();

    var threads = ArrayList(Thread).init(main_allocator.allocator());
    defer threads.deinit();

    try threads.append(try dbg.serveRequestsForever());

    var gui_arena = ArenaAllocator.init(main_allocator.allocator());
    defer gui_arena.deinit();
    try gui.run(gui_arena.allocator(), dbg);

    log.info("shutting down");

    for (threads.items) |thread| thread.join();
}

fn openOrCreateLogFile(file_path: []const u8) !fs.File {
    if (fs.openFileAbsolute(file_path, fs.File.OpenFlags{ .mode = .read_write })) |fp| {
        // the file exists, read until the end to we don't overwrite its contents
        const stat = try fp.stat();
        try fp.seekTo(stat.size);
        return fp;
    } else |err| {
        switch (err) {
            error.FileNotFound => {
                // the file does not exist, so create it
                return try fs.createFileAbsolute(file_path, fs.File.CreateFlags{});
            },
            else => {
                return err;
            },
        }
    }
}

test {
    //
    // Allow logging and the file cache to leak memory and the log FD in tests
    //

    var tsa = heap.c_allocator.create(ThreadSafeAllocator) catch unreachable;
    tsa.* = ThreadSafeAllocator{ .child_allocator = heap.c_allocator };

    const log_fp = switch (flags.CI) {
        true => std.io.getStdOut(),
        false => try openOrCreateLogFile("/tmp/inspect.log"),
    };

    logging.init(.{
        .allocator = tsa.allocator(),
        .level = .dbg,
        .regions = "all",
        .fp = log_fp,
    }) catch unreachable;

    file.initHashCache(tsa.allocator());

    comptime {
        std.testing.refAllDeclsRecursive(@This());
        std.testing.refAllDeclsRecursive(@import("test/simulator.zig"));
    }
}
