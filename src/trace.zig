const std = @import("std");
const Src = std.builtin.SourceLocation;

const spall = @import("trace/spall.zig").Spall;

const tracy = @import("ztracy");

pub fn init() void {
    spall.init();
}

pub fn deinit() void {
    spall.deinit();
}

pub fn initThread() void {
    spall.initThread();
}

pub fn deinitThread() void {
    spall.deinitThread();
}

/// @NOTE (jrc): not supported by Spall
pub fn message(msg: []const u8) void {
    tracy.Message(msg);
}

pub fn zone(comptime src: Src) ZoneCtx {
    spall.zone(src);
    return .{ .tracy_ctx = tracy.Zone(src) };
}

pub fn zoneN(comptime src: Src, comptime name: [:0]const u8) ZoneCtx {
    spall.zoneN(name);
    return .{ .tracy_ctx = tracy.ZoneN(src, name) };
}

pub const ZoneCtx = struct {
    tracy_ctx: tracy.ZoneCtx,

    pub fn end(self: ZoneCtx) void {
        spall.end();
        self.tracy_ctx.End();
    }
};
