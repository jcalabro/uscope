const std = @import("std");
const builtin = @import("builtin");
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

const endian = builtin.cpu.arch.endian();

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
    _ = params;

    // const name = params.data_type_name;

    // // string slices
    // if (params.data_type.form == .@"struct" and strings.eql(params.data_type_name, "[]u8")) {
    //     if (readUsizeStructMember(params, "len") catch null) |len| return len.data;
    //     return null;
    // }

    // // string literals (i.e. *[13:0]u8)
    // if (params.data_type.form == .pointer and
    //     mem.startsWith(u8, name, "*[") and mem.endsWith(u8, name, ":0]u8"))
    // {
    //     var num_str = mem.trimLeft(u8, name, "*[");
    //     num_str = mem.trimRight(u8, num_str, ":0]u8");
    //     return fmt.parseInt(u64, num_str, 10) catch |err| {
    //         log.warnf("unable to parse zig string length: {!}", .{err});
    //         return 0;
    //     };
    // }

    return null;
}

/// Read Zig-style strings, which are a byte slice whose length we determine from the type name
fn renderString(
    params: *const encoding.Params,
    len: u64,
) encoding.EncodeVariableError!encoding.RenderStringResult {
    _ = params;
    _ = len;

    unreachable;

    // const addr = types.Address.from(mem.readInt(u64, @ptrCast(params.val), endian));

    // var str = ArrayListUnmanaged(u8){};
    // const max_str_len = math.pow(usize, 2, 12);
    // for (0..max_str_len) |ndx| {
    //     var buf = [_]u8{0};
    //     params.adapter.peekData(
    //         params.pid,
    //         params.load_addr,
    //         addr.addInt(ndx),
    //         &buf,
    //     ) catch {
    //         return error.ReadDataError;
    //     };

    //     if (buf[0] == 0) break;

    //     try str.append(params.scratch, buf[0]);
    //     if (ndx == max_str_len - 1) try str.appendSlice(params.scratch, "...");

    //     if (len > 0 and ndx > len) break;
    // }

    // return .{
    //     .address = addr,
    //     .str = try str.toOwnedSlice(params.scratch),
    // };
}

// fn memberNameIs(params: *const encoding.Params, name: strings.Hash, comptime expected: String) bool {
//     const name_str = params.target_strings.get(name) orelse return false;
//     return strings.eql(name_str, expected);
// }

fn isSlice(params: *const encoding.Params) bool {
    _ = params;
    return false;

    // return switch (params.base_data_type.form) {
    //     .@"struct" => |strct| strct.members.len == 2 and
    //         memberNameIs(params, strct.members[0].name, "ptr") and
    //         memberNameIs(params, strct.members[1].name, "len"),
    //     else => false,
    // };
}

// const UsizeStructMemberResult = struct {
//     data: usize,
//     data_type: types.TypeNdx,
// };

// fn readUsizeStructMember(
//     params: *const encoding.Params,
//     comptime name: String,
// ) encoding.EncodeVariableError!UsizeStructMemberResult {
//     var data_type: ?types.TypeNdx = null;
//     const member_offset_bytes = blk: {
//         for (params.data_type.form.@"struct".members) |m| {
//             const name_str = params.target_strings.get(m.name) orelse continue;
//             data_type = m.data_type;
//             if (strings.eql(name, name_str)) break :blk m.offset_bytes;
//         }

//         log.warn("slice struct does not contain a member named " ++ name);
//         return error.ReadDataError;
//     };

//     const start = member_offset_bytes;
//     const end = start + @sizeOf(usize);
//     const data = mem.readInt(usize, @ptrCast(params.val[start..end]), endian);

//     return .{
//         .data = data,
//         .data_type = data_type.?,
//     };
// }

fn renderSlice(params: *const encoding.Params) encoding.EncodeVariableError!encoding.RenderSliceResult {
    _ = params;

    unreachable;

    // // read the address of the buffer and its length
    // const ptr = try readUsizeStructMember(params, "ptr");
    // const addr = types.Address.from(ptr.data);
    // const len = (try readUsizeStructMember(params, "len")).data;

    // // find the number of bytes taken by one element in the buffer
    // const member_size_bytes = blk: {
    //     for (params.data_type.form.@"struct".members) |m| {
    //         const name_str = params.target_strings.get(m.name) orelse continue;
    //         if (strings.eql("ptr", name_str)) {
    //             var base_data_type = params.cu.data_types[m.data_type.int()];
    //             while (base_data_type.form == .pointer) {
    //                 if (base_data_type.form.pointer.data_type) |ptr_type| {
    //                     base_data_type = params.cu.data_types[ptr_type.int()];
    //                     continue;
    //                 }
    //                 break;
    //             }

    //             break :blk base_data_type.size_bytes;
    //         }
    //     }

    //     log.warn("slice struct does not contain a member named ptr");
    //     return error.ReadDataError;
    // };

    // // @TODO (jrc): allow the user to configure the max preview length in their settings, or accept this as a
    // // parameter on the render expression, i.e. `myslice | len=1000` or similar
    // const preview_len = @min(len, 100);

    // const full_buf = try params.scratch.alloc(u8, preview_len * member_size_bytes);
    // params.adapter.peekData(params.pid, params.load_addr, addr, full_buf) catch {
    //     return error.ReadDataError;
    // };

    // var item_bufs = try params.scratch.alloc(String, preview_len);
    // for (0..preview_len) |ndx| {
    //     const start = ndx * member_size_bytes;
    //     const end = start + member_size_bytes;
    //     item_bufs[ndx] = full_buf[start..end];
    // }

    // // dereference the pointer (i.e. []u32 -> *u32 -> u32)
    // const ptr_t = params.cu.data_types[ptr.data_type.int()];
    // const ptr_data_type = switch (ptr_t.form) {
    //     .pointer => |p| p.data_type,
    //     else => return error.InvalidDataType,
    // };

    // return encoding.RenderSliceResult{
    //     .address = addr,
    //     .len = len,
    //     .item_data_type = ptr_data_type,
    //     .item_bufs = item_bufs,
    // };
}
