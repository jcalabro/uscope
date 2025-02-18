const std = @import("std");
const Allocator = std.mem.Allocator;
const ArrayListUnmanaged = std.ArrayListUnmanaged;

const C = @import("C.zig");
const encoding = @import("encoding.zig");
const logging = @import("../../logging.zig");
const strings = @import("../../strings.zig");
const String = strings.String;

const log = logging.Logger.init(logging.Region.Debugger);

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

fn isOpaquePointer(params: *const encoding.Params) bool {
    return strings.eql(params.data_type_name, "rawptr");
}

fn isString(params: *const encoding.Params) ?u64 {
    // null-terminated c strings
    if (params.data_type.form == .pointer and strings.eql(params.data_type_name, "cstring")) {
        return 0;
    }

    // string slices
    if (params.data_type.form == .@"struct" and strings.eql(params.data_type_name, "string")) {
        const res = encoding.readUsizeStructMember(params, "len") catch |err| {
            log.errf("unable to read odin string length: {!}", .{err});
            return null;
        };
        return res.data;
    }

    return null;
}

/// Read C-style null terminated strings
fn renderString(
    params: *const encoding.Params,
    len: u64,
) encoding.EncodeVariableError!encoding.RenderStringResult {
    return try C.renderString(params, len);
}

fn isSlice(params: *const encoding.Params) bool {
    return switch (params.data_type.form) {
        .@"struct" => |strct| strct.members.len == 2 and
            encoding.memberNameIs(params, strct.members[0].name, "data") and
            encoding.memberNameIs(params, strct.members[1].name, "len"),
        else => false,
    };
}

fn renderSlice(params: *const encoding.Params) encoding.EncodeVariableError!encoding.RenderSliceResult {
    return encoding.renderSlice("data", "len", params);
}
