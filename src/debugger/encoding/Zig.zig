const std = @import("std");
const Allocator = std.mem.Allocator;
const ArrayListUnmanaged = std.ArrayListUnmanaged;
const fmt = std.fmt;
const math = std.math;
const mem = std.mem;

const encoding = @import("encoding.zig");
const logging = @import("../../logging.zig");
const strings = @import("../../strings.zig");
const String = strings.String;
const types = @import("../../types.zig");

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
    return strings.eql(params.data_type_name, "*anyopaque");
}

fn isString(params: *const encoding.Params) ?u64 {
    const name = params.data_type_name;

    // string slices
    if (params.data_type.form == .@"struct" and strings.eql(params.data_type_name, "[]u8")) {
        if (encoding.readUsizeStructMember(params, "len") catch null) |len| return len.data;
        return null;
    }

    // string literals (i.e. *[13:0]u8)
    if (params.data_type.form == .pointer and
        mem.startsWith(u8, name, "*[") and mem.endsWith(u8, name, ":0]u8"))
    {
        var num_str = mem.trimLeft(u8, name, "*[");
        num_str = mem.trimRight(u8, num_str, ":0]u8");
        return fmt.parseInt(u64, num_str, 10) catch |err| {
            log.warnf("unable to parse zig string length: {!}", .{err});
            return 0;
        };
    }

    return null;
}

/// Read Zig-style strings, which are a byte slice whose length we determine from the type name
fn renderString(
    params: *const encoding.Params,
    len: u64,
) encoding.EncodeVariableError!encoding.RenderStringResult {
    const addr = types.Address.from(mem.readInt(u64, @ptrCast(params.val), encoding.endian));

    var str = ArrayListUnmanaged(u8){};
    const max_str_len = math.pow(usize, 2, 12);
    for (0..max_str_len) |ndx| {
        var buf = [_]u8{0};
        params.adapter.peekData(
            params.pid,
            params.load_addr,
            addr.addInt(ndx),
            &buf,
        ) catch {
            return error.ReadDataError;
        };

        if (buf[0] == 0) break;

        try str.append(params.scratch, buf[0]);
        if (ndx == max_str_len - 1) try str.appendSlice(params.scratch, "...");

        if (len > 0 and ndx > len) break;
    }

    return .{
        .address = addr,
        .str = try str.toOwnedSlice(params.scratch),
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
