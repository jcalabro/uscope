const std = @import("std");
const builtin = @import("builtin");
const Allocator = std.mem.Allocator;
const ArrayListUnmanaged = std.ArrayListUnmanaged;
const mem = std.mem;

const encoding = @import("encoding.zig");
const strings = @import("../../strings.zig");
const types = @import("../../types.zig");

const endian = builtin.cpu.arch.endian();

pub fn encoder() encoding.Encoding {
    return .{
        .isString = isString,
        .renderString = renderString,
        .isSlice = isSlice,
        .renderSlice = renderSlice,
    };
}

fn isString(params: *const encoding.Params) ?usize {
    if (params.data_type.form == .pointer and strings.eql(params.base_data_type_name, "char")) {
        // we don't know the length of null-terminated C strings ahead of time
        return 0;
    }

    return null;
}

/// C doesn't have the notion of a "slice" built in
fn isSlice(_: *const encoding.Params) bool {
    return false;
}

fn renderSlice(_: *const encoding.Params) encoding.EncodeVariableError!encoding.RenderSliceResult {
    unreachable;
}

/// Read C-style strings one byte at a time until we encounter a null terminator
fn renderString(
    params: *const encoding.Params,
    len: u64,
) encoding.EncodeVariableError!encoding.RenderStringResult {
    const addr = types.Address.from(mem.readInt(u64, @ptrCast(params.val), endian));

    var str = ArrayListUnmanaged(u8){};
    const max_str_len = 256;
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
    };
}
