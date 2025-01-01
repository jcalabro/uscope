const std = @import("std");
const builtin = @import("builtin");
const assert = std.debug.assert;
const flags = @import("../flags.zig");
const Src = std.builtin.SourceLocation;

const spall = @cImport({
    @cInclude("spall.h");
});

const SpallType = switch (flags.SpallEnabled) {
    true => SpallCtx,
    false => SpallNoopCtx,
};

pub const Spall = struct {
    pub fn init() void {
        SpallType.init();
    }

    pub fn deinit() void {
        SpallType.deinit();
    }

    pub fn initThread() void {
        SpallType.initThread();
    }

    pub fn deinitThread() void {
        SpallType.deinitThread();
    }

    pub fn zone(src: Src) void {
        SpallType.zone(src);
    }

    pub fn zoneN(name: []const u8) void {
        SpallType.zoneN(name);
    }

    pub fn end() void {
        SpallType.end();
    }
};

const SpallCtx = struct {
    var spall_ctx: spall.SpallProfile = undefined;

    const len = 8 * 1024 * 1048;
    threadlocal var buf = [_]u8{0} ** len;
    threadlocal var spall_buf: spall.SpallBuffer = undefined;

    threadlocal var pid: u32 = 0;
    threadlocal var tid: u32 = 0;

    fn init() void {
        spall_ctx = spall.spall_init_file_json("trace.spall", 1);
    }

    fn deinit() void {
        spall.spall_quit(&spall_ctx);
    }

    fn initThread() void {
        switch (builtin.os.tag) {
            .linux => {
                pid = @intCast(std.os.linux.getpid());
                tid = @intCast(std.os.linux.gettid());
            },
            else => @compileError("unsupported platform"),
        }

        spall_buf = .{
            .length = buf.len,
            .data = &buf,
        };

        assert(spall.spall_buffer_init(&spall_ctx, &spall_buf));
    }

    fn deinitThread() void {
        assert(spall.spall_buffer_quit(&spall_ctx, &spall_buf));
    }

    fn zone(src: Src) void {
        zoneN(src.fn_name);
    }

    fn zoneN(name: []const u8) void {
        assert(spall.spall_buffer_begin_ex(
            &spall_ctx,
            &spall_buf,
            name.ptr,
            @intCast(name.len),
            getTimeInMicros(),
            tid,
            pid,
        ));
    }

    fn end() void {
        assert(spall.spall_buffer_end_ex(
            &spall_ctx,
            &spall_buf,
            getTimeInMicros(),
            tid,
            pid,
        ));
    }

    fn getTimeInMicros() f64 {
        const ts = std.time.microTimestamp();
        return @floatFromInt(ts);
    }
};

const SpallNoopCtx = struct {
    fn init() void {}

    fn deinit() void {}

    fn initThread() void {}

    fn deinitThread() void {}

    fn zone(_: Src) void {}

    fn zoneN(_: []const u8) void {}

    fn end() void {}
};
