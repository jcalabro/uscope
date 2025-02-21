const std = @import("std");
const builtin = @import("builtin");
const Allocator = mem.Allocator;
const mem = std.mem;

const Adapter = @import("../debugger.zig").Adapter;
const logging = @import("../../logging.zig");
const strings = @import("../../strings.zig");
const String = strings.String;
const types = @import("../../types.zig");

const log = logging.Logger.init(logging.Region.Debugger);

pub const endian = builtin.cpu.arch.endian();

pub const Params = struct {
    /// Must be a scratch arena allocator. Caller owns all returned memory.
    scratch: Allocator,

    adapter: *Adapter,
    pid: types.PID,
    cu: *const types.CompileUnit,
    target_strings: *strings.Cache,

    data_type: *const types.DataType,
    data_type_name: String,

    base_data_type: *const types.DataType,
    base_data_type_name: String,

    /// The initial variable value as raw bytes as returned from the Adapter for the target platform
    val: String,
};

pub const EncodeVariableError = error{InvalidDataType} || error{ReadDataError} || Allocator.Error;

/// Encoding is a comptime-known interface type that allows us to select how to read data from memory based
/// on the programming language of the compile unit we're inspecting. These Encoding objects are thin wrappers
/// that are not designed to have long lifetimes. All allocators passed to these functions must be scratch
/// arenas, and the caller owns returned memory.
pub const Encoding = struct {
    const Self = @This();

    isOpaquePointer: *const fn (params: *const Params) bool,

    /// Returns null in the case that the symbol is not a string. Returns 0 if the length is unknown. Else,
    /// returns the length of the string as noted in the debug symbols.
    ///
    /// @UX (jrc): should this be a user-configurable setting? Should it come
    /// as a display modifier on the watch expression, i.e. `str|len=100`?
    isString: *const fn (params: *const Params) ?u64,
    renderString: *const fn (params: *const Params, len: u64) EncodeVariableError!RenderStringResult,

    isSlice: *const fn (params: *const Params) bool,
    renderSlice: *const fn (params: *const Params) EncodeVariableError!RenderSliceResult,
};

pub const RenderStringResult = struct {
    /// The address that points to the start of the string in the subordinate's memory
    address: types.Address,

    /// A preview of the string (this may be shorter than the actual string)
    str: String,

    /// The final calculated size of the full string (not just the preview). This is null in
    /// the case of very long null-terminated strings.
    len: ?usize,
};

pub const RenderSliceResult = struct {
    /// The address that points to the start of the slice's buffer in the subordinate's memory
    address: types.Address,

    /// The total number of items in the slice
    len: usize,

    /// The type of each element in the slice. If the pointer in the slice is pointing to an
    /// opaque type, `item_data_type` will be null, and we can't render a preview.
    item_data_type: ?types.TypeNdx,

    /// A preview of the slice items (this may be shorter than the actual slice)
    item_bufs: []String,
};

pub const UsizeStructMemberResult = struct {
    data: usize,
    data_type: types.TypeNdx,
};

pub fn readUsizeStructMember(
    params: *const Params,
    comptime name: String,
) EncodeVariableError!UsizeStructMemberResult {
    var data_type: ?types.TypeNdx = null;
    const member_offset_bytes = blk: {
        for (params.data_type.form.@"struct".members) |m| {
            const name_str = params.target_strings.get(m.name) orelse continue;
            data_type = m.data_type;
            if (strings.eql(name, name_str)) break :blk m.offset_bytes;
        }

        log.warn("slice struct does not contain a member named " ++ name);
        return error.ReadDataError;
    };

    const start = member_offset_bytes;
    const end = start + @sizeOf(usize);
    const data = mem.readInt(usize, @ptrCast(params.val[start..end]), endian);

    return .{
        .data = data,
        .data_type = data_type.?,
    };
}

pub fn memberNameIs(params: *const Params, name: strings.Hash, comptime expected: String) bool {
    const name_str = params.target_strings.get(name) orelse return false;
    return strings.eql(name_str, expected);
}

pub fn renderSlice(
    comptime data_name: String,
    comptime len_name: String,
    params: *const Params,
) EncodeVariableError!RenderSliceResult {
    // read the address of the buffer and its length
    const ptr = try readUsizeStructMember(params, data_name);
    const addr = types.Address.from(ptr.data);
    const len = (try readUsizeStructMember(params, len_name)).data;

    // find the number of bytes taken by one element in the buffer
    const member_size_bytes = blk: {
        for (params.data_type.form.@"struct".members) |m| {
            const name_str = params.target_strings.get(m.name) orelse continue;
            if (strings.eql(data_name, name_str)) {
                var base_data_type = params.cu.data_types[m.data_type.int()];

                // follow pointers and typedefs to their base type
                var done = false;
                while (!done) {
                    switch (base_data_type.form) {
                        .pointer => |p| {
                            if (p.data_type) |ptr_type|
                                base_data_type = params.cu.data_types[ptr_type.int()];
                        },

                        .typedef => |td| {
                            if (td.data_type) |td_type|
                                base_data_type = params.cu.data_types[td_type.int()];
                        },

                        else => done = true,
                    }
                }

                while (base_data_type.form == .pointer) {
                    break;
                }

                break :blk base_data_type.size_bytes;
            }
        }

        log.warn("slice struct does not contain a member named " ++ data_name);
        return error.ReadDataError;
    };

    // @TODO (jrc): allow the user to configure the max preview length in their settings, or accept this as a
    // parameter on the render expression, i.e. `myslice | len=1000` or similar
    const preview_len = @min(len, 100);

    const full_buf = try params.scratch.alloc(u8, preview_len * member_size_bytes);
    params.adapter.peekData(params.pid, types.Address.from(0), addr, full_buf) catch {
        return error.ReadDataError;
    };

    var item_bufs = try params.scratch.alloc(String, preview_len);
    for (0..preview_len) |ndx| {
        const start = ndx * member_size_bytes;
        const end = start + member_size_bytes;
        item_bufs[ndx] = full_buf[start..end];
    }

    // dereference the pointer (i.e. []u32 -> *u32 -> u32)
    const ptr_t = params.cu.data_types[ptr.data_type.int()];
    const ptr_data_type = switch (ptr_t.form) {
        .pointer => |p| p.data_type,
        else => return error.InvalidDataType,
    };

    return RenderSliceResult{
        .address = addr,
        .len = len,
        .item_data_type = ptr_data_type,
        .item_bufs = item_bufs,
    };
}
