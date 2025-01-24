const std = @import("std");
const builtin = @import("builtin");
const Allocator = std.mem.Allocator;
const t = std.testing;
const Thread = std.Thread;

const flags = @import("flags.zig");
const logging = @import("logging.zig");

const log = logging.Logger.init(logging.Region.Misc);

pub fn setThreadName(thread: Thread, comptime name: []const u8) void {
    // don't set thread names when distributing to customers
    if (flags.Release) return;

    thread.setName(name) catch {};
}

pub fn enumFromInt(comptime enumT: anytype, val: anytype) error{UnexpectedValue}!enumT {
    return inline for (@typeInfo(enumT).@"enum".fields) |f| {
        if (val == f.value) break @enumFromInt(f.value);
    } else {
        log.errf("invalid " ++ @typeName(enumT) ++ " enum value: 0x{x}", .{val});
        return error.UnexpectedValue;
    };
}

pub fn enumFromIntSilent(comptime enumT: anytype, val: anytype) error{UnexpectedValue}!enumT {
    return inline for (@typeInfo(enumT).@"enum".fields) |f| {
        if (val == f.value) break @enumFromInt(f.value);
    } else {
        return error.UnexpectedValue;
    };
}

test "safe.enumFromInt" {
    const MyEnum = enum(u8) { one, two, three };

    try t.expectEqual(MyEnum.one, try enumFromInt(MyEnum, @intFromEnum(MyEnum.one)));
    try t.expectEqual(MyEnum.one, try enumFromIntSilent(MyEnum, @intFromEnum(MyEnum.one)));
    try t.expectEqual(MyEnum.two, try enumFromInt(MyEnum, @intFromEnum(MyEnum.two)));
    try t.expectEqual(MyEnum.two, try enumFromIntSilent(MyEnum, @intFromEnum(MyEnum.two)));
    try t.expectEqual(MyEnum.three, try enumFromInt(MyEnum, @intFromEnum(MyEnum.three)));
    try t.expectEqual(MyEnum.three, try enumFromIntSilent(MyEnum, @intFromEnum(MyEnum.three)));

    try t.expectEqual(error.UnexpectedValue, enumFromInt(MyEnum, 3));
    try t.expectEqual(error.UnexpectedValue, enumFromIntSilent(MyEnum, 3));
    try t.expectEqual(error.UnexpectedValue, enumFromInt(MyEnum, 4));
    try t.expectEqual(error.UnexpectedValue, enumFromIntSilent(MyEnum, 4));
    try t.expectEqual(error.UnexpectedValue, enumFromInt(MyEnum, 1000));
    try t.expectEqual(error.UnexpectedValue, enumFromIntSilent(MyEnum, 1000));
    try t.expectEqual(error.UnexpectedValue, enumFromInt(MyEnum, -1));
    try t.expectEqual(error.UnexpectedValue, enumFromIntSilent(MyEnum, -1));
}

pub fn optional(comptime T: type, opt: ?T) error{UnexpectedOptional}!T {
    if (opt) |o| return o;
    return error.UnexpectedOptional;
}

pub fn optionalWithErrLog(comptime T: type, opt: ?T) error{UnexpectedOptional}!T {
    return try optionalWithLog(T, opt, .err);
}

pub fn optionalWithLog(comptime T: type, opt: ?T, log_lvl: logging.Level) error{UnexpectedOptional}!T {
    if (opt) |o| return o;

    const msg = "optional of type \"{s}\" not found";
    switch (log_lvl) {
        .dbg => log.debugf(msg, .{@typeName(@TypeOf(T))}),
        .inf => log.infof(msg, .{@typeName(@TypeOf(T))}),
        .wrn => log.warnf(msg, .{@typeName(@TypeOf(T))}),
        .err => log.errf(msg, .{@typeName(@TypeOf(T))}),
        .ftl => log.fatalf(msg, .{@typeName(@TypeOf(T))}),
    }

    return error.UnexpectedOptional;
}

test "optional" {
    var item: ?i32 = null;
    try t.expectError(error.UnexpectedOptional, optional(i32, item));
    try t.expectError(error.UnexpectedOptional, optionalWithLog(i32, item, .dbg));

    item = 123;
    try t.expectEqual(item.?, try optional(i32, item));
    try t.expectEqual(item.?, try optionalWithLog(i32, item, .dbg));
}

const default_float_tolerance: f32 = 0.001;

pub fn floatsEql(a: f32, b: f32) bool {
    return floatsEqlTolerance(a, b, default_float_tolerance);
}

pub fn floatsEqlTolerance(a: f32, b: f32, tolerance: f32) bool {
    return @abs(a - b) <= tolerance;
}

test "float tolerance" {
    try t.expect(floatsEql(1.0, 1.0));
    try t.expect(floatsEql(-5.0, -5.0));
    try t.expect(!floatsEql(1.0, 1.1));
    try t.expect(!floatsEql(1.0, -1.0));
}

/// Copies the contents of a slice to a new slice stored with the given allocator
// @RENAME (jrc): clone
pub fn copySlice(comptime T: type, alloc: Allocator, src: []const T) Allocator.Error![]T {
    const copy = try alloc.alloc(T, src.len);
    @memcpy(copy, src);
    return copy;
}
