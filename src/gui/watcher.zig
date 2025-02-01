const std = @import("std");
const builtin = @import("builtin");
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;
const Thread = std.Thread;
const time = std.time;

const logging = @import("../logging.zig");
const safe = @import("../safe.zig");
const State = @import("State.zig");
const trace = @import("../trace.zig");

const log = logging.Logger.init(logging.Region.GUI);

pub const Watcher = switch (builtin.os.tag) {
    .linux => LinuxWatcher,
    else => @compileError("platform  not supported"),
};

const LinuxWatcher = struct {
    const IN = std.os.linux.IN;
    const posix = std.posix;

    ifd: i32 = undefined,
    wd: i32 = undefined,

    fpath: []const u8,

    state: *State,
    callback: *const fn (*State) void,

    pub fn init(
        alloc: Allocator,
        fpath: []const u8,
        state: *State,
        callback: *const fn (*State) void,
    ) !*LinuxWatcher {
        const self = try alloc.create(LinuxWatcher);
        self.* = LinuxWatcher{
            .fpath = try safe.copySlice(u8, alloc, fpath),
            .state = state,
            .callback = callback,
        };

        // don't listen for file changes in tests
        if (builtin.is_test) return self;

        try self.setupWatch();

        const thread = try Thread.spawn(.{}, pollEvents, .{self});
        safe.setThreadName(thread, "LinuxWatcher.pollEvents");

        return self;
    }

    pub fn deinit(self: *LinuxWatcher, alloc: Allocator) void {
        // @TODO (jrc): stop background thread
        alloc.free(self.fpath);
        alloc.destroy(self);
    }

    fn setupWatch(self: *LinuxWatcher) !void {
        self.ifd = try posix.inotify_init1(IN.NONBLOCK);
        self.wd = try posix.inotify_add_watch(self.ifd, self.fpath, IN.CLOSE_WRITE);
    }

    fn pollEvents(self: *LinuxWatcher) void {
        trace.initThread();
        defer trace.deinitThread();

        while (true) {
            var fds = [_]posix.pollfd{.{
                .fd = self.ifd,
                .events = posix.POLL.IN,
                .revents = 0,
            }};

            const poll = posix.poll(@ptrCast(&fds), -1) catch |err| {
                log.warnf("unable to poll for file descriptor changes: {!}", .{err});
                continue;
            };

            if (poll <= 0) continue;

            // @NOTE (jrc): a sleep is required for the file to flush to disk (this is pretty janky...)
            time.sleep(50 * time.ns_per_ms);

            const max = std.math.pow(usize, 2, 10);
            for (0..max) |ndx| {
                // read the data, though we don't need to do anything with it
                var buf = [_]u8{0} ** 4096;
                _ = posix.read(self.ifd, @ptrCast(&buf)) catch break;

                const buf_slice: []const u8 = @ptrCast(&buf);
                const event = std.mem.bytesAsValue(std.os.linux.inotify_event, buf_slice.ptr);

                if ((event.mask & IN.IGNORED) != 0) {
                    // IGNORED indicates that the watch was explicitly
                    // removed, so we need to re-initialize it
                    self.setupWatch() catch |err| {
                        log.errf("unable to re-establish file watch after IN_IGNORED: {!}", .{err});
                        break;
                    };
                }

                assert(ndx < max - 1);
            }

            self.callback(self.state);
        }
    }
};
