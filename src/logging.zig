const std = @import("std");
const Allocator = std.mem.Allocator;
const eql = mem.eql;
const fs = std.fs;
const io = std.io;
const mem = std.mem;
const Mutex = std.Thread.Mutex;
const testing = std.testing;

const time = @import("time");

const BufferedWriter = io.BufferedWriter(4096, fs.File.Writer);

pub const Region = struct {
    pub const Debugger = "debugger";
    pub const GUI = "gui";
    pub const Linux = "linux";
    pub const Main = "main";
    pub const Misc = "misc";
    pub const Settings = "settings";
    pub const Test = "test";
    pub const Types = "types";
    pub const Symbols = "symbols";
};

pub const Level = enum(u8) {
    dbg,
    inf,
    wrn,
    err,
    ftl,

    pub fn fromStr(lvl: []const u8) error{InvalidLogLevel}!@This() {
        if (eql(u8, lvl, "ftl") or eql(u8, lvl, "fatal")) {
            return .ftl;
        }
        if (eql(u8, lvl, "err") or eql(u8, lvl, "error")) {
            return .err;
        }
        if (eql(u8, lvl, "wrn") or eql(u8, lvl, "warn") or eql(u8, lvl, "warning")) {
            return .wrn;
        }
        if (eql(u8, lvl, "inf") or eql(u8, lvl, "info")) {
            return .inf;
        }
        if (eql(u8, lvl, "dbg") or eql(u8, lvl, "debug")) {
            return .dbg;
        }

        return error.InvalidLogLevel;
    }

    fn enabled(global_lvl: @This(), local_lvl: @This()) bool {
        return @intFromEnum(global_lvl) <= @intFromEnum(local_lvl);
    }
};

test "log levels" {
    try testing.expect(Level.enabled(.dbg, .dbg));
    try testing.expect(Level.enabled(.dbg, .inf));
    try testing.expect(Level.enabled(.wrn, .err));
    try testing.expect(Level.enabled(.ftl, .ftl));

    try testing.expect(!Level.enabled(.inf, .dbg));
    try testing.expect(!Level.enabled(.wrn, .inf));
    try testing.expect(!Level.enabled(.ftl, .err));
}

pub const Options = struct {
    allocator: Allocator,

    level: Level = Level.dbg,

    regions: []const u8 = "all",
    active_regions: [][]const u8 = undefined,

    fp: fs.File,
    buf: BufferedWriter = undefined,

    color: bool = true,

    // only one thread is allowed to write to the output file at a time
    mu: Mutex = Mutex{},

    pub fn deinit(self: *@This()) void {
        // attempt a best-effort flush before shutting down
        self.buf.flush() catch {};

        self.allocator.free(self.active_regions);
    }
};

// opts must be set at startup via one call to init(), then never modifided later
var opts: Options = undefined;

pub fn init(op: Options) !void {
    opts = op;
    opts.buf = io.bufferedWriter(opts.fp.writer());

    opts.active_regions = blk: {
        var active_regions = std.ArrayList([]const u8).init(opts.allocator);
        errdefer active_regions.deinit();

        // split up regions based on "," and trim remaining whitespace
        const whitespace = " \r\t\x00";
        var iter = mem.splitSequence(u8, opts.regions, ",");
        try active_regions.append(iter.first());
        var next = iter.next();
        while (next != null) {
            const region = mem.trim(u8, next.?, whitespace);
            try active_regions.append(region);
            next = iter.next();
        }

        break :blk try active_regions.toOwnedSlice();
    };
}

pub fn deinit() void {
    opts.deinit();
}

pub const Color = enum {
    Red,
    Green,
    Yellow,
    Blue,
    Reset,

    pub fn str(self: Color) []const u8 {
        return switch (self) {
            .Red => "\x1b[31m",
            .Green => "\x1b[32m",
            .Yellow => "\x1b[33m",
            .Blue => "\x1b[34m",
            .Reset => "\x1b[0m",
        };
    }
};

pub const Logger = struct {
    const Self = @This();

    region: []const u8,

    pub fn init(comptime region: []const u8) Self {
        return Self{
            .region = region,
        };
    }

    fn write(self: Self, comptime fmt: []const u8, args: anytype, lvl: Level, color: Color) void {
        if (!Level.enabled(opts.level, lvl)) {
            return;
        }

        var ok = false;
        for (opts.active_regions) |region| {
            if (mem.eql(u8, region, "all") or mem.eql(u8, region, self.region)) {
                ok = true;
                break;
            }
        }
        if (!ok) {
            // region is not enabled
            return;
        }

        const color_str = switch (opts.color) {
            true => color.str(),
            false => Color.Reset.str(),
        };

        // @PERFORMANCE (jrc): can we get a small improvement by only calling allocPrint once below?
        const msg = std.fmt.allocPrint(opts.allocator, fmt, args) catch return;
        defer opts.allocator.free(msg);

        // RFC 3999 (i.e. 2023-01-19T08:37:01, in UTC since that's all that time.zig supports)
        const time_fmt = "YYYY-MM-DDTHH:mm:ss";
        var now = [_]u8{0} ** time_fmt.len;
        var buf = io.fixedBufferStream(&now);
        time.DateTime.now().format(time_fmt, .{}, buf.writer()) catch return;

        // [level] [region] [time] message
        const output = std.fmt.allocPrint(opts.allocator, "{s}[{s}] [{s}] [{s}]{s} {s}\n", .{
            .color = color_str,
            .level = @tagName(lvl),
            .region = self.region,
            .now = now,
            .reset = Color.Reset.str(), // reset to the default terminal color
            .msg = msg,
        }) catch return;
        defer opts.allocator.free(output);

        opts.mu.lock();
        defer opts.mu.unlock();

        opts.buf.writer().writeAll(output) catch return;
    }

    pub fn debug(self: Self, comptime fmt: []const u8) void {
        debugf(self, fmt, .{});
    }

    pub fn debugf(self: Self, comptime fmt: []const u8, args: anytype) void {
        write(self, fmt, args, Level.dbg, Color.Green);
    }

    pub fn info(self: Self, comptime fmt: []const u8) void {
        infof(self, fmt, .{});
    }

    pub fn infof(self: Self, comptime fmt: []const u8, args: anytype) void {
        write(self, fmt, args, Level.inf, Color.Blue);
    }

    pub fn warn(self: Self, comptime fmt: []const u8) void {
        warnf(self, fmt, .{});
    }

    pub fn warnf(self: Self, comptime fmt: []const u8, args: anytype) void {
        write(self, fmt, args, Level.wrn, Color.Yellow);
    }

    pub fn err(self: Self, comptime fmt: []const u8) void {
        errf(self, fmt, .{});
    }

    pub fn errf(self: Self, comptime fmt: []const u8, args: anytype) void {
        write(self, fmt, args, Level.err, Color.Red);
    }

    pub fn fatal(self: Self, comptime fmt: []const u8) noreturn {
        fatalf(self, fmt, .{});
    }

    pub fn fatalf(self: Self, comptime fmt: []const u8, args: anytype) noreturn {
        write(self, fmt, args, Level.ftl, Color.Red);
        self.flush();
        std.process.exit(1);
    }

    pub fn flush(_: Self) void {
        opts.mu.lock();
        defer opts.mu.unlock();

        opts.buf.flush() catch return;
    }
};
