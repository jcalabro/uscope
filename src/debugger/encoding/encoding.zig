const std = @import("std");
const Allocator = std.mem.Allocator;

const Adapter = @import("../debugger.zig").Adapter;
const strings = @import("../../strings.zig");
const String = strings.String;
const types = @import("../../types.zig");

pub const Params = struct {
    /// Must be a scratch arena allocator. Caller owns all returned memory.
    scratch: Allocator,

    adapter: *Adapter,
    pid: types.PID,
    load_addr: types.Address,
    cu: *const types.CompileUnit,
    target_strings: *strings.Cache,

    data_type: *const types.DataType,
    data_type_name: String,

    base_data_type: *const types.DataType,
    base_data_type_name: String,

    /// The initial variable value as raw bytes as returned from the Adapter for the target platform
    val: []const u8,
};

pub const EncodeVariableError = error{ReadDataError} || Allocator.Error;

/// Encoding is a comptime-known interface type that allows us to select how to read data from memory based
/// on the programming language of the compile unit we're inspecting. These Encoding objects are thin wrappers
/// that are not designed to have long lifetimes. All allocators passed to these functions must be scratch
/// arenas, and the caller owns returned memory.
pub const Encoding = struct {
    const Self = @This();

    /// Returns null in the case that the symbol is not a string. Returns 0 if the length is unknown. Else,
    /// returns the length of the string as noted in the debug symbols.
    ///
    /// @UX (jrc): should this be a user-configurable setting? Should it come
    /// as a display modifier on the watch expression, i.e. `str|len=100`?
    isString: *const fn (params: *const Params) ?u64,
    renderString: *const fn (params: *const Params, len: u64) EncodeVariableError!RenderStringResult,

    isSlice: *const fn (params: *const Params) bool,
    renderSlice: *const fn (params: *const Params) EncodeVariableError!RenderStringResult,
};

pub const RenderStringResult = struct {
    address: types.Address,
    str: String,
};
