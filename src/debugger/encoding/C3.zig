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
    return strings.eql(params.data_type_name, "void*");
}

fn isString(params: *const encoding.Params) ?u64 {
    // string slices
    if (params.data_type.form == .@"struct" and strings.eql(params.data_type_name, "char[]")) {
        const res = encoding.readUsizeStructMember(params, "len") catch |err| {
            log.errf("unable to read c3 string length: {!}", .{err});
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
    const res = try encoding.renderSlice("ptr", "len", params);

    const buf = try params.scratch.alloc(u8, res.item_bufs.len);
    for (res.item_bufs, 0..) |item, ndx| {
        buf[ndx] = item[0];
    }

    return encoding.RenderStringResult{
        .address = res.address,
        .str = buf,
        .len = len,
    };
}

fn isSlice(params: *const encoding.Params) bool {
    return switch (params.data_type.form) {
        .@"struct" => |strct| strct.members.len == 2 and
            encoding.memberNameIs(params, strct.members[0].name, "ptr") and
            encoding.memberNameIs(params, strct.members[1].name, "len"),
        else => false,
    };
}

fn renderSlice(params: *const encoding.Params) encoding.EncodeVariableError!encoding.RenderSliceResult {
    return encoding.renderSlice("ptr", "len", params);
}
