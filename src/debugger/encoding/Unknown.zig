const std = @import("std");

const encoding = @import("encoding.zig");
const strings = @import("../../strings.zig");
const types = @import("../../types.zig");

const Self = @This();

pub fn encoder() encoding.Encoding {
    return encoding.Encoding{
        .isOpaquePointer = isOpaquePointer,
        .isString = isString,
        .renderString = renderString,
        .isSlice = isSlice,
        .renderSlice = renderSlice,
    };
}

fn isOpaquePointer(_: *const encoding.Params) bool {
    return false;
}

fn isString(_: *const encoding.Params) ?usize {
    return null;
}

fn isSlice(_: *const encoding.Params) bool {
    return false;
}

fn renderSlice(_: *const encoding.Params) encoding.EncodeVariableError!encoding.RenderSliceResult {
    unreachable;
}

fn renderString(_: *const encoding.Params, _: u64) encoding.EncodeVariableError!encoding.RenderStringResult {
    unreachable;
}
