//! Contains code for safely parsing DWARF debug symbols from binaries

const std = @import("std");
const builtin = @import("builtin");
const Allocator = mem.Allocator;
const ArenaAllocator = heap.ArenaAllocator;
const ArrayList = std.ArrayList;
const ArrayListUnmanaged = std.ArrayListUnmanaged;
const assert = std.debug.assert;
const AutoHashMap = std.AutoHashMap;
const AutoHashMapUnmanaged = std.AutoHashMapUnmanaged;
const fmt = std.fmt;
const heap = std.heap;
const mem = std.mem;
const pow = std.math.pow;
const t = std.testing;
const Thread = std.Thread;
const ThreadSafeAllocator = std.heap.ThreadSafeAllocator;
const TypeMap = AutoHashMap(Offset, usize);

const abbrev = @import("dwarf/abbrev.zig");
const aranges = @import("dwarf/aranges.zig");
const consts = @import("dwarf/consts.zig");
const file_utils = @import("../file.zig");
const flags = @import("../flags.zig");
const frame = @import("dwarf/frame.zig");
const info = @import("dwarf/info.zig");
const line = @import("dwarf/line.zig");
const logging = @import("../logging.zig");
const MainAllocator = @import("../MainAllocator.zig");
const Queue = @import("../queue.zig").Queue;
const Reader = @import("../Reader.zig");
const safe = @import("../safe.zig");
const strings = @import("../strings.zig");
const String = strings.String;
const trace = @import("../trace.zig");
const types = @import("../types.zig");

const log = logging.Logger.init(logging.Region.Symbols);

/// Contains the list of supported DWARF versions
pub const Version = enum(u4) {
    // Not officially supported for DIEs, but call frame info may emit V1, which we do support
    one = 1,
    // Not officially supported for DIEs, but call frame info may emit V2, which we do support
    two = 2,

    three = 3,
    four = 4,
    five = 5,

    pub fn int(self: @This()) u8 {
        return @intFromEnum(self);
    }

    pub fn isLessThan(self: @This(), version: @This()) bool {
        return @intFromEnum(self) < @intFromEnum(version);
    }

    pub fn isAtLeast(self: @This(), version: @This()) bool {
        return @intFromEnum(self) >= @intFromEnum(version);
    }
};

/// Offset is a common integer primitive in DWARF debug info. Its size depends on
/// whether or not the CU is using 32 or 64 bit debug information. 32 bit mode
/// is overwhelmingly more common, though we support 64 bit mode as well
/// @TODO (jrc): make this a `types.Numeric`
pub const Offset = u64;

pub const Section = struct {
    addr: u64,
    contents: []const u8,
};

/// Sections are pointers to all the sections in the binary that
/// are required to parse DWARF debug info
pub const Sections = struct {
    //
    // Required sections
    //

    /// Describes the shape of the symbols in .debug_info
    abbrev: Section,

    /// Primary section containing debug symbols
    info: Section,

    /// Source line information mappings
    line: Section,

    //
    // Optional sections
    //

    /// (optional) Mapping from memory address to function compilation units in .debug_info
    aranges: Section,

    /// (optional) Contains call frame tables. Note that there are slight differences between .debug_frame and .eh_frame.
    frame: Section,

    /// (optional) Contains call frame tables. Note that there are slight differences between .debug_frame and .eh_frame.
    eh_frame: Section,

    /// (optional) Location lists used to describe location of variable values over time
    loc: Section,

    /// (optional) Macro information
    macinfo: Section,

    /// (optional) Maps function names to compilation unit debug info
    pubnames: Section,

    /// (optional) Maps type names to compilation unit debug info
    pubtypes: Section,

    /// (optional) Mapping from memory address to debug info
    ranges: Section,

    /// (optional) Strings used by other sections
    str: Section,

    /// (optional) Introduced in DWARF v4, then merged back in to .debug_info in v5
    types: Section,

    /// (optional) Introduced in DWARF v5. A string section specific to storing memory addresses.
    addr: Section,

    /// (optional) Introduced in DWARF v5. A string section specific to the line number table.
    line_str: Section,

    /// (optional) Introduced in DWARF v5. Replaces the .debug_loc section with a more efficient data
    /// format whose purpose is to store location list programs that compute the location of a variable
    /// over time.
    loclists: Section,

    /// (optional) Introduced in DWARF v5. Replaces the .debug_macinfo with a format that can be more compact.
    macro: Section,

    /// (optional) Introduced in DWARF v5. Replaces the .debug_pubnames and .debug_pubtypes sections
    /// with a more functional name index section.
    names: Section,

    /// (optional) Introduced in DWARF v5. Replaces the .debug_ranges section with a more effecient data
    /// format whose purpose is to map memory addresses to debug info.
    rnglists: Section,

    /// (optional) Introduced in DWARF v5. Contains the string offsets table for the strings in the
    /// .debug_str section
    str_offsets: Section,
};

pub const ParseError = error{
    InvalidDWARFInfo,
    InvalidDWARFVersion,
    LanguageUnsupported,
    OutOfMemory,
} || Thread.SpawnError;

/// Options for parsing DWARF symbols from binaries
pub const ParseOpts = struct {
    /// Must be an arena allocator and must be deinit'ed by the caller after us
    scratch: Allocator,

    /// Global cache of absolute file paths
    file_cache: *file_utils.Cache,

    /// The full contents of each DWARF section. Each must be set by the caller
    /// if the section exists in the binary.
    sections: *const Sections,
};

/// applyOffset safely applies the given offset, accounting for signed-ness. It does not
/// protect against integer underflow or overflow in ReleaseFast builds.
pub fn applyOffset(from: u64, offset: i128) u64 {
    if (offset >= 0) {
        const unsigned: u64 = @intCast(offset);
        return from + unsigned;
    }

    const signed: i64 = @intCast(offset);
    const unsigned: u64 = @intCast(-signed);
    return from - unsigned;
}

test "dwarf.applyOffset" {
    try t.expectEqual(0, applyOffset(0, 0));
    try t.expectEqual(0, applyOffset(10, -10));
    try t.expectEqual(20, applyOffset(10, 10));
}

/// In DWARF, many fields are of an ambiguous length depending on whether or not
/// we're in 32 or 64 bit mode. In 32-bit mode, lengths are 4 bytes. In 64-bit
/// mode, lengths are 12 bytes on disk, but the first four bytes are 0xffffffff.
/// This function determines which mode we're in, reads the appropriate amount of
/// data, and returns the result as the larger 64 bit type.
pub fn readInitialLength(r: *Reader) ParseError!Offset {
    const val = try read(r, u32);
    if (val == 0xffffffff) {
        return try read(r, u64);
    }

    return val;
}

/// Reads @sizeOf(T) bytes from the given reader
pub fn read(r: *Reader, comptime T: type) error{InvalidDWARFInfo}!T {
    return r.read(T) catch |err| {
        log.errf("unable to read " ++ @typeName(T) ++ ": {!}", .{err});
        return error.InvalidDWARFInfo;
    };
}

/// Reads from r in to the given buffer
pub fn readBuf(r: *Reader, buf: []u8) error{InvalidDWARFInfo}!usize {
    return r.readBuf(buf) catch |err| {
        log.errf("unable to read buf of length {d}: {!}", .{ buf.len, err });
        return error.InvalidDWARFInfo;
    };
}

/// Reads until the byte "token" is encountered
pub fn readUntil(r: *Reader, token: u8) error{InvalidDWARFInfo}![]const u8 {
    return r.readUntil(token) catch |err| {
        log.errf("unable to read until token 0x{x}: {!}", .{ token, err });
        return error.InvalidDWARFInfo;
    };
}

/// Reads a ULEB128 from the given reader (max of 10 bytes of data)
pub fn readULEB128(r: *Reader) error{InvalidDWARFInfo}!u64 {
    return r.readULEB128() catch |err| {
        log.errf("unable to read uleb128: {!}", .{err});
        return error.InvalidDWARFInfo;
    };
}

/// Reads a SLEB128 from the given reader (max of 10 bytes of data)
pub fn readSLEB128(r: *Reader) error{InvalidDWARFInfo}!i64 {
    return r.readSLEB128() catch |err| {
        log.errf("unable to read sleb128: {!}", .{err});
        return error.InvalidDWARFInfo;
    };
}

/// Reads an address of the given size and returns it as a u64
pub fn readAddr(r: *Reader, addr_size: types.AddressSize) ParseError!u64 {
    return switch (addr_size) {
        .four => @intCast(try read(r, u32)),
        .eight => try read(r, u64),
    };
}

/// Reads @sizeOf(sizeT) bytes as the given type and checks the value is a valid member of the enum (even in ReleaseFast builds)
pub fn readEnum(r: *Reader, comptime sizeT: anytype, comptime enumT: anytype) error{InvalidDWARFInfo}!enumT {
    return safeEnumFromInt(enumT, try read(r, sizeT));
}

/// Reads a ULEB128 and checks the value is a valid member of the enum (even in ReleaseFast builds)
pub fn readEnumULEB128(r: *Reader, comptime enumT: anytype) ParseError!enumT {
    return safeEnumFromInt(enumT, try readULEB128(r));
}

/// Reads a SLEB128 and checks the value is a valid member of the enum (even in ReleaseFast builds)
pub fn readEnumSLEB128(r: *Reader, comptime enumT: anytype) ParseError!enumT {
    return safeEnumFromInt(enumT, try readSLEB128(r));
}

/// Safely checks that the given value can be converted to a member of the enum
pub fn safeEnumFromInt(comptime enumT: anytype, num: anytype) error{InvalidDWARFInfo}!enumT {
    return safe.enumFromInt(enumT, num) catch return error.InvalidDWARFInfo;
}

pub const TableOffsets = struct {
    debug_str_offsets: usize = 0,
    debug_addr: usize = 0,
};

fn validateSectionNotEmpty(comptime name: String, section: String) ParseError!void {
    if (section.len == 0) {
        log.errf("unable to parse dwarf: {s} is empty", .{name});
        return error.InvalidDWARFInfo;
    }
}

/// Parses the DWARF symbols from the given binary in to a platform-agnostic type. `perm_alloc` must be
/// a ThreadSafeAllocator under the hood.
pub fn parse(perm_alloc: Allocator, opts: *const ParseOpts, target: *types.Target) ParseError!void {
    const z = trace.zoneN(@src(), "parse dwarf");
    defer z.end();

    try validateSectionNotEmpty(".debug_info", opts.sections.info.contents);
    try validateSectionNotEmpty(".debug_abbrev", opts.sections.abbrev.contents);
    try validateSectionNotEmpty(".debug_line", opts.sections.line.contents);

    const abbrev_tables = try abbrev.parse(opts);

    var partial_compile_units = ArrayList(PartiallyReadyCompileUnit).init(opts.scratch);
    var data_types = ArrayList(types.DataType).init(opts.scratch);

    {
        const z1 = trace.zoneN(@src(), "parse compile units");
        defer z1.end();

        var offset: usize = 0;
        var offsets = TableOffsets{};
        const max = pow(usize, 2, 18);
        for (0..max) |cu_ndx| {
            if (offset >= opts.sections.info.contents.len) break;

            // parse the compile unit header
            const dwarf_cu = try info.CompileUnit.create(opts, offset);
            try dwarf_cu.parseHeader(abbrev_tables, &offsets);
            if (dwarf_cu.header.addr_size.bytes() > target.addr_size.bytes()) {
                target.addr_size = dwarf_cu.header.addr_size;
            }

            // parse all DIEs for this compile unit
            const dies = try dwarf_cu.parseDIEs();
            offset += dwarf_cu.info_r.offset();

            // map to a (partially ready) generic type
            const partial_cu = try mapDWARFToTarget(dwarf_cu, dies, &data_types);
            try partial_compile_units.append(partial_cu);

            // copy strings to permanent memory since they're all known at this time
            try target.strings.map.ensureTotalCapacity(
                target.strings.alloc,
                target.strings.map.size + partial_cu.strings.map.size,
            );
            var str_it = partial_cu.strings.map.iterator();
            while (str_it.next()) |str| {
                target.strings.map.putAssumeCapacity(
                    str.key_ptr.*,
                    try strings.clone(target.strings.alloc, str.value_ptr.*),
                );
            }

            assert(cu_ndx <= max - 1);
        }
    }

    //
    // Perform delayed type resolution. This is needed because DWARF type
    // offsets are often given out-of-order, and some compiler (i.e. Go)
    // actually use offsets from the beginning of the `.debug_info` section
    // rather than the beginning of the compile unit, so we need to parse
    // ALL compile units, then perform type resolution.
    //

    {
        const z1 = trace.zoneN(@src(), "delayed type resolution");
        defer z1.end();

        for (partial_compile_units.items) |*partial_cu| {
            // Const types
            for (partial_cu.delayed_refs.const_types.items) |const_type| {
                const data_type_ndx = dt: {
                    const offset_map = try mapForVariableTypeEntry(partial_compile_units.items, partial_cu, const_type);
                    if (offset_map.get(const_type.type_offset)) |dt_ndx| {
                        if (dt_ndx.int() < data_types.items.len) {
                            break :dt dt_ndx;
                        }
                    }

                    log.errf("unable to find data type for const with offset 0x{x} in compile unit with offset 0x{x}", .{
                        const_type.type_offset,
                        partial_cu.delayed_refs.offset_range.low,
                    });
                    return error.InvalidDWARFInfo;
                };

                const constant = &data_types.items[const_type.variable_ndx.int()];
                constant.*.form.constant.data_type = data_type_ndx;
            }

            // Typedef types
            for (partial_cu.delayed_refs.typedef_types.items) |typedef_type| {
                const data_type_ndx = dt: {
                    const offset_map = try mapForVariableTypeEntry(partial_compile_units.items, partial_cu, typedef_type);
                    if (offset_map.get(typedef_type.type_offset)) |dt_ndx| {
                        if (dt_ndx.int() < data_types.items.len) {
                            break :dt dt_ndx;
                        }
                    }

                    log.errf("unable to find data type for typedef with offset 0x{x} in compile unit with offset 0x{x}", .{
                        typedef_type.type_offset,
                        partial_cu.delayed_refs.offset_range.low,
                    });
                    return error.InvalidDWARFInfo;
                };

                const td = &data_types.items[typedef_type.variable_ndx.int()];
                td.*.form.typedef.data_type = data_type_ndx;
            }

            // Struct/class/union member types
            // first, assign all types to members
            for (partial_cu.delayed_refs.struct_member_types.items) |member_type| {
                const data_type_ndx = dt: {
                    const offset_map = try mapForVariableTypeEntry(partial_compile_units.items, partial_cu, member_type);
                    if (offset_map.get(member_type.type_offset)) |dt_ndx| {
                        if (dt_ndx.int() < data_types.items.len) {
                            break :dt dt_ndx;
                        }
                    }

                    log.errf("unable to find data type for struct member with offset 0x{x} in compile unit with offset 0x{x}", .{
                        member_type.type_offset,
                        partial_cu.delayed_refs.offset_range.low,
                    });
                    return error.InvalidDWARFInfo;
                };

                partial_cu.delayed_refs.struct_members.items[member_type.struct_ndx]
                    .members.items[member_type.member_ndx].data_type = data_type_ndx;
            }
            // then, assign all member arrays to structs/classes/unions
            for (partial_cu.delayed_refs.struct_members.items) |*members| {
                const data_type = &data_types.items[members.struct_ndx.int()];
                switch (data_type.*.form) {
                    .@"struct" => |*s| s.members = try members.members.toOwnedSlice(),
                    .@"union" => |*u| u.members = try members.members.toOwnedSlice(),
                    else => unreachable,
                }
            }

            // Pointer types
            for (partial_cu.delayed_refs.pointer_types.items) |ptr_type| {
                if (ptr_type.type_offset == null) continue;

                const data_type_ndx = dt: {
                    const offset_map = try mapForVariableTypeEntry(partial_compile_units.items, partial_cu, ptr_type);
                    if (offset_map.get(ptr_type.type_offset.?)) |dt_ndx| {
                        if (dt_ndx.int() < data_types.items.len) {
                            break :dt dt_ndx;
                        }
                    }

                    log.errf("unable to find data type for pointer with offset 0x{x} in compile unit with offset 0x{x}", .{
                        ptr_type.type_offset.?,
                        partial_cu.delayed_refs.offset_range.low,
                    });
                    return error.InvalidDWARFInfo;
                };

                const ptr = &data_types.items[ptr_type.variable_ndx.int()];
                ptr.*.form.pointer.data_type = data_type_ndx;

                // pointer name may or may not already be set at this time
                const ptr_name = target.strings.get(ptr.name);
                if (ptr_name == null or ptr_name.?.len == 0) {
                    const data_type = data_types.items[data_type_ndx.int()];
                    const item_type_name = target.strings.get(data_type.name) orelse types.Unknown;
                    const type_name = try types.PointerType.nameFromItemType(opts.scratch, item_type_name);
                    ptr.*.name = try target.strings.add(type_name);
                }
            }

            // Array types
            for (partial_cu.delayed_refs.array_types.items) |arr_type| {
                const data_type_ndx = dt: {
                    const offset_map = try mapForVariableTypeEntry(partial_compile_units.items, partial_cu, arr_type);
                    if (offset_map.get(arr_type.type_offset)) |dt_ndx| {
                        if (dt_ndx.int() < data_types.items.len) {
                            break :dt dt_ndx;
                        }
                    }

                    log.errf("unable to find data type for array with offset 0x{x} in compile unit with offset 0x{x}", .{
                        arr_type.type_offset,
                        partial_cu.delayed_refs.offset_range.low,
                    });
                    return error.InvalidDWARFInfo;
                };

                const data_type = data_types.items[data_type_ndx.int()];
                const item_type_name = target.strings.get(data_type.name) orelse types.Unknown;
                const type_name = try types.ArrayType.nameFromItemType(opts.scratch, item_type_name);

                const arr_ptr = &data_types.items[arr_type.variable_ndx.int()];
                arr_ptr.*.form.array.element_type = data_type_ndx;
                arr_ptr.*.name = try target.strings.add(type_name);

                arr_ptr.*.size_bytes = 0;
                if (arr_ptr.form.array.len) |len| {
                    arr_ptr.*.size_bytes = data_type.size_bytes * len;
                }
            }

            // Variable types
            for (partial_cu.delayed_refs.variable_types.items) |vt| {
                const data_type_ndx = dt: {
                    const offset_map = try mapForVariableTypeEntry(partial_compile_units.items, partial_cu, vt);
                    if (offset_map.get(vt.type_offset)) |dt_ndx| {
                        if (dt_ndx.int() < data_types.items.len) {
                            break :dt dt_ndx;
                        }
                    }

                    log.errf("unable to find data type for variable with offset 0x{x} in compile unit with offset 0x{x}", .{
                        vt.type_offset,
                        partial_cu.delayed_refs.offset_range.low,
                    });
                    return error.InvalidDWARFInfo;
                };

                const var_ptr = &partial_cu.cu.variables[vt.variable_ndx.int()];
                var_ptr.*.data_type = data_type_ndx;
            }
        }
    }

    {
        const z1 = trace.zoneN(@src(), "copy compile units");
        defer z1.end();

        // copy from scratch to permanent memory
        var generic_compile_units = ArrayList(types.CompileUnit).init(perm_alloc);
        errdefer generic_compile_units.deinit();

        for (partial_compile_units.items) |src| {
            const perm_cu = try generic_compile_units.addOne();
            try perm_cu.copyFrom(perm_alloc, src.cu);
        }

        target.compile_units = try generic_compile_units.toOwnedSlice();
    }

    {
        const z1 = trace.zoneN(@src(), "copy data types");
        defer z1.end();

        var perm_data_types = ArrayList(types.DataType).init(perm_alloc);
        errdefer {
            for (perm_data_types.items) |dt| dt.deinit(perm_alloc);
            perm_data_types.deinit();
        }

        for (data_types.items) |dt| {
            var dt_copy = try perm_data_types.addOne();
            try dt_copy.copyFrom(perm_alloc, dt);
        }

        target.data_types = try perm_data_types.toOwnedSlice();
    }

    target.unwinder = .{ .cies = try frame.loadTable(perm_alloc, opts) };
}

const OffsetTypeMap = AutoHashMapUnmanaged(Offset, types.TypeNdx);

/// DWARF often gives us type info out-of-order, so we need to track which data needs its
/// references resolved once all compile units have been parsed. Much of the time, we could
/// just resolve all references, but some compilers (i.e. Go) use FormClass.reference, which
/// means we should be using global offsets (GOFF in dwarfdump) from the start of the entire
/// `.debug_info` section rather than from the start of the current compile unit. This is why
/// we can't easily use a parallel DWARF parser unfortunately.
const DelayedReferences = struct {
    const OffsetRange = struct {
        low: Offset,
        high: Offset,

        // Checks if `offset` is contained in [low, high)
        fn contains(self: @This(), offset: Offset) bool {
            return offset >= self.low and offset < self.high;
        }
    };

    /// The start offset of this compile unit
    offset_range: OffsetRange,

    /// Reverse mapping of Offset -> type index within a single compile unit. The Offset
    /// in this case is an offset from the start of the compile unit DIE.
    data_type_map: OffsetTypeMap = .{},

    /// Reverse mapping of Offset -> type index across all compile units. The Offset
    /// in this case is an offset from the start of the .debug_info section.
    global_data_type_map: OffsetTypeMap = .{},

    variable_types: ArrayListUnmanaged(VariableTypeEntry) = .{},
    array_types: ArrayListUnmanaged(VariableTypeEntry) = .{},
    const_types: ArrayListUnmanaged(VariableTypeEntry) = .{},
    typedef_types: ArrayListUnmanaged(VariableTypeEntry) = .{},
    pointer_types: ArrayListUnmanaged(PointerTypeEntry) = .{},
    struct_member_types: ArrayListUnmanaged(StructMemberTypeEntry) = .{},
    struct_members: ArrayListUnmanaged(StructMemberListEntry) = .{},

    fn addDataType(
        self: *@This(),
        cu: *const info.CompileUnit,
        die: *const info.DIE,
        data_types: *const ArrayList(types.DataType),
    ) Allocator.Error!void {
        try self.data_type_map.put(
            cu.opts.scratch,
            die.offset,
            types.TypeNdx.from(data_types.items.len),
        );

        try self.global_data_type_map.put(
            cu.opts.scratch,
            cu.info_offset + die.offset,
            types.TypeNdx.from(data_types.items.len),
        );
    }
};

/// We accumulate DW_AT_type (an offset) mapped to the index in the variables array in
/// this temporary buffer so we can stitch type information together after the first pass
const VariableTypeEntry = struct {
    /// Whether or not `type_offset` is an offset from the start of the compile unit or
    /// an offset from the start of the .debug_info section
    is_global_offset: bool,
    type_offset: Offset,
    variable_ndx: types.VariableNdx,
};

/// Find the appropriate reverse mapping collection (Offset -> TypeNdx) for the given
/// variable type. This can either be the current `PartiallyReadyCompileUnit`, or a
/// different one altogether if the variable's type references a type definition in a
/// different CU.
fn mapForVariableTypeEntry(
    partial_compile_units: []PartiallyReadyCompileUnit,
    partial_cu: *const PartiallyReadyCompileUnit,
    type_entry: anytype,
) error{InvalidDWARFInfo}!OffsetTypeMap {
    if (!type_entry.is_global_offset) return partial_cu.delayed_refs.data_type_map;

    const offset = o: {
        if (comptime @TypeOf(type_entry.type_offset) == ?Offset) {
            break :o type_entry.type_offset.?;
        } else {
            break :o type_entry.type_offset;
        }
    };

    for (partial_compile_units) |pcu| {
        if (pcu.delayed_refs.offset_range.contains(offset)) {
            return pcu.delayed_refs.global_data_type_map;
        }
    }

    log.errf("no compilation unit found for type with global offset 0x{x}", .{offset});
    return error.InvalidDWARFInfo;
}

/// Pointers types are nullable in the case that a pointer is opaque
/// (i.e. for *u8, we want to set its reference type to u8)
const PointerTypeEntry = struct {
    /// Whether or not `type_offset` is an offset from the start of the compile unit or
    /// an offset from the start of the .debug_info section
    is_global_offset: bool,
    type_offset: ?Offset,
    variable_ndx: types.VariableNdx,
};

const StructMemberTypeEntry = struct {
    /// Whether or not `type_offset` is an offset from the start of the compile unit or
    /// an offset from the start of the .debug_info section
    is_global_offset: bool,
    /// the offset of the type of the member
    type_offset: Offset,
    /// the offset to the struct in the struct_members list
    struct_ndx: usize,
    /// the index of the member within the struct's member list
    member_ndx: usize,
};

const StructMemberListEntry = struct {
    struct_ndx: types.TypeNdx,
    members: ArrayList(types.MemberType),
};

const PartiallyReadyCompileUnit = struct {
    cu: types.CompileUnit,
    strings: *strings.Cache,
    delayed_refs: DelayedReferences,
};

/// Converts the info.CompileUnit to a types.CompileUnit. All memory is allocated in the thread's
/// scratch allocator, so the returned type needs to be copied back to the main thread's perm alloc.
fn mapDWARFToTarget(
    cu: *info.CompileUnit,
    dies: []const info.DIE,
    data_types: *ArrayList(types.DataType),
) ParseError!PartiallyReadyCompileUnit {
    const z = trace.zone(@src());
    defer z.end();

    const str_cache = try strings.Cache.init(cu.opts.scratch);

    var language = types.Language.Unsupported;
    var ranges: []types.AddressRange = &.{};
    var sources: []types.SourceFile = &.{};

    var variables = ArrayList(types.Variable).init(cu.opts.scratch);
    var function_variables = ArrayList(ArrayList(types.VariableNdx)).init(cu.opts.scratch);

    var functions = ArrayList(types.Function).init(cu.opts.scratch);
    var function_ranges = ArrayList(types.CompileUnit.Functions.Range).init(cu.opts.scratch);

    var delayed_refs = DelayedReferences{ .offset_range = .{
        .low = cu.info_offset,
        .high = cu.info_offset + cu.header.total_len,
    } };

    {
        const z1 = trace.zoneN(@src(), "map DIEs");
        defer z1.end();

        var die_ndx: usize = 0;
        while (die_ndx < dies.len) : (die_ndx += 1) {
            const z2 = trace.zoneN(@src(), "map one DIE");
            defer z2.end();

            var opts = AttributeParseOpts{ .cu = cu, .die = &dies[die_ndx] };

            switch (opts.die.tag) {
                .DW_TAG_compile_unit => {
                    language = try (blk: {
                        const producer = try requiredAttribute(&opts, []const u8, .DW_AT_producer);
                        if (consts.Language.fromProducer(producer)) |l| {
                            break :blk l;
                        } else {
                            break :blk try requiredAttribute(&opts, consts.Language, .DW_AT_language);
                        }
                    }).toGeneric();

                    // offset in to the .debug_line section where this CU's line program lives
                    if (try optionalAttribute(&opts, u64, .DW_AT_stmt_list)) |line_offset| {
                        // DW_AT_comp_dir is optional if all filenames are absolute in the .debug_line table
                        const comp_dir = try optionalAttribute(&opts, []const u8, .DW_AT_comp_dir) orelse "";
                        sources = try line.parse(&opts, &cu.source_abs_path_hashes, comp_dir, line_offset);
                    }

                    // parse .debug_aranges
                    var range_opts = aranges.ParseOpts{
                        .opts = &opts,
                        .sources = sources,
                        .func_statements = null,
                    };
                    ranges = try aranges.parse(&range_opts);

                    // @TODO (jrc): parse .debug_rnglist
                },

                .DW_TAG_subprogram => {
                    var func_statements = ArrayList(types.SourceStatement).init(cu.opts.scratch);
                    var range_opts = aranges.ParseOpts{
                        .opts = &opts,
                        .sources = sources,
                        .func_statements = &func_statements,
                    };
                    const func_ranges = try aranges.parse(&range_opts);

                    for (func_ranges) |r| {
                        try function_ranges.append(.{
                            .range = r,
                            .func_ndx = types.FunctionNdx.from(functions.items.len),
                        });
                    }

                    const name = name: {
                        if (try optionalAttribute(&opts, String, .DW_AT_linkage_name)) |linkage_name| {
                            break :name try str_cache.add(linkage_name);
                        }
                        break :name try parseAndCacheString(&opts, .DW_AT_name, str_cache);
                    };

                    try functions.append(.{
                        .name = name,
                        .source_loc = try parseSourceLoc(&opts),
                        .statements = try func_statements.toOwnedSlice(),
                        .addr_ranges = func_ranges,
                        .inlined_function_indices = &.{}, // @TODO (jrc): this needs to be added
                        .variables = undefined, // will be assigned later
                        .platform_data = switch (builtin.os.tag) {
                            .linux => .{
                                .frame_base = try parseAndCacheString(&opts, .DW_AT_frame_base, str_cache),
                            },
                            else => @compileError("build target not supported"),
                        },
                    });

                    try function_variables.append(ArrayList(types.VariableNdx).init(cu.opts.scratch));
                },

                // type declarations for function pointers
                .DW_TAG_subroutine_type => {
                    try delayed_refs.addDataType(cu, opts.die, data_types);
                    try data_types.append(.{
                        .size_bytes = opts.cu.header.addr_size.bytes(),
                        .name = try parseAndCacheString(&opts, .DW_AT_name, str_cache),
                        .form = types.DataTypeForm{ .function = types.FunctionType{} },
                    });
                },

                // variables are locals, formals parameters are function parameters
                .DW_TAG_variable, .DW_TAG_formal_parameter => b: {
                    const name = try optionalAttribute(&opts, String, .DW_AT_name) orelse "";
                    if (name.len == 0) break :b;

                    if (try optionalAttributeWithForm(&opts, Offset, .DW_AT_type)) |type_offset| {
                        try delayed_refs.variable_types.append(cu.opts.scratch, .{
                            .is_global_offset = type_offset.isGlobalOffset(),
                            .type_offset = type_offset.data,
                            .variable_ndx = types.VariableNdx.from(variables.items.len),
                        });
                    }

                    if (function_variables.items.len > 0) {
                        // assign this variable to its containing function
                        const last = function_variables.items.len - 1;
                        try function_variables.items[last].append(
                            types.VariableNdx.from(variables.items.len),
                        );
                    }

                    try variables.append(.{
                        .name = try str_cache.add(name),
                        .data_type = undefined, // will be assigned later
                        .platform_data = switch (builtin.os.tag) {
                            .linux => .{
                                .location_expression = try parseLocationExpression(&opts, str_cache),
                            },
                            else => @compileError("build target not supported"),
                        },
                    });
                },

                .DW_TAG_unspecified_type => {
                    try delayed_refs.addDataType(cu, opts.die, data_types);
                    try data_types.append(.{
                        .size_bytes = 0,
                        .name = try parseAndCacheString(&opts, .DW_AT_name, str_cache),
                        .form = types.DataTypeForm{ .unknown = types.UnknownType{} },
                    });
                },

                .DW_TAG_base_type => {
                    try delayed_refs.addDataType(cu, opts.die, data_types);
                    try data_types.append(.{
                        .size_bytes = try requiredAttribute(&opts, u32, .DW_AT_byte_size),
                        .name = try parseAndCacheString(&opts, .DW_AT_name, str_cache),
                        .form = types.DataTypeForm{ .primitive = types.PrimitiveType{
                            .encoding = try parseTypeEncoding(&opts),
                        } },
                    });
                },

                .DW_TAG_const_type => {
                    if (try optionalAttributeWithForm(&opts, Offset, .DW_AT_type)) |type_offset| {
                        try delayed_refs.const_types.append(cu.opts.scratch, .{
                            .is_global_offset = type_offset.isGlobalOffset(),
                            .type_offset = type_offset.data,
                            .variable_ndx = types.VariableNdx.from(data_types.items.len),
                        });
                    }

                    try delayed_refs.addDataType(cu, opts.die, data_types);
                    try data_types.append(.{
                        .size_bytes = 0, // callers must follow the const to get the size of this type
                        .name = try parseAndCacheString(&opts, .DW_AT_name, str_cache),
                        .form = types.DataTypeForm{
                            .constant = types.ConstantType{
                                .data_type = null, // will be assigned later, if a type is defined for this const
                            },
                        },
                    });
                },

                .DW_TAG_typedef => {
                    if (try optionalAttributeWithForm(&opts, Offset, .DW_AT_type)) |type_offset| {
                        try delayed_refs.typedef_types.append(cu.opts.scratch, .{
                            .is_global_offset = type_offset.isGlobalOffset(),
                            .type_offset = type_offset.data,
                            .variable_ndx = types.VariableNdx.from(data_types.items.len),
                        });
                    }

                    try delayed_refs.addDataType(cu, opts.die, data_types);
                    try data_types.append(.{
                        .size_bytes = 0, // callers must follow the typedef to get the size of this type
                        .name = try parseAndCacheString(&opts, .DW_AT_name, str_cache),
                        .form = types.DataTypeForm{
                            .typedef = types.TypedefType{
                                .data_type = null, // will be assigned later if a DW_AT_type is provided
                            },
                        },
                    });
                },

                // @NOTE (jrc): I'm not sure it's correct to treat volatile types the same as
                // pointers...but the data is shaped very similarly
                .DW_TAG_pointer_type,
                .DW_TAG_reference_type,
                .DW_TAG_restrict_type,
                .DW_TAG_ptr_to_member_type,
                .DW_TAG_rvalue_reference_type,
                .DW_TAG_volatile_type,
                => {
                    // DW_TAG_pointer_type without a DW_AT_type just tells us how large
                    // a pointer size is on this target, which we don't care about
                    const ptr_type = try optionalAttributeWithForm(&opts, Offset, .DW_AT_type);
                    try delayed_refs.pointer_types.append(cu.opts.scratch, .{
                        .is_global_offset = if (ptr_type) |p| p.isGlobalOffset() else false,
                        .type_offset = if (ptr_type) |p| p.data else null,
                        .variable_ndx = types.VariableNdx.from(data_types.items.len),
                    });

                    const num_bytes = cu.header.addr_size.bytes();

                    try delayed_refs.addDataType(cu, opts.die, data_types);
                    try data_types.append(.{
                        .size_bytes = try optionalAttribute(&opts, u32, .DW_AT_byte_size) orelse num_bytes,
                        .name = try parseAndCacheString(&opts, .DW_AT_name, str_cache),
                        .form = types.DataTypeForm{
                            .pointer = types.PointerType{
                                .data_type = undefined, // will be assigned later
                            },
                        },
                    });
                },

                .DW_TAG_array_type => {
                    if (try optionalAttributeWithForm(&opts, Offset, .DW_AT_type)) |type_offset| {
                        try delayed_refs.array_types.append(cu.opts.scratch, .{
                            .is_global_offset = type_offset.isGlobalOffset(),
                            .type_offset = type_offset.data,
                            .variable_ndx = types.VariableNdx.from(data_types.items.len),
                        });
                    } else {
                        log.errf("unknown array type for DIE at offset 0x{x}", .{opts.die.offset});
                        return error.InvalidDWARFInfo;
                    }

                    const len = l: {
                        // array len information is stored in a child DIE
                        die_ndx += 1;
                        if (die_ndx >= dies.len) {
                            log.errf("unable to get array array type at offset 0x{x}", .{opts.die.offset});
                            return error.InvalidDWARFInfo;
                        }

                        const arr_opts = AttributeParseOpts{ .cu = cu, .die = &dies[die_ndx] };
                        if (try optionalAttribute(&arr_opts, usize, .DW_AT_upper_bound)) |upper| {
                            break :l upper + 1;
                        }
                        if (try optionalAttribute(&arr_opts, usize, .DW_AT_count)) |count| {
                            break :l count;
                        }
                        break :l null; // array len is unknown
                    };

                    try delayed_refs.addDataType(cu, opts.die, data_types);
                    try data_types.append(.{
                        .size_bytes = 0,
                        .name = try parseAndCacheString(&opts, .DW_AT_name, str_cache),
                        .form = types.DataTypeForm{
                            .array = types.ArrayType{
                                .len = len,
                                .element_type = undefined, // will be assigned later
                            },
                        },
                    });
                },

                .DW_TAG_structure_type, .DW_TAG_class_type, .DW_TAG_union_type => {
                    const original_die_ndx = die_ndx;
                    defer die_ndx = original_die_ndx;

                    var members = ArrayList(types.MemberType).init(cu.opts.scratch);
                    const max_members = pow(usize, 2, 12);
                    var member_ndx: usize = 0;
                    done: for (0..max_members) |ndx| {
                        die_ndx += 1;
                        if (die_ndx >= dies.len) break; // we've hit the end of the DIE list

                        const member_opts = AttributeParseOpts{ .cu = cu, .die = &dies[die_ndx] };
                        switch (member_opts.die.tag) {
                            .DW_TAG_member => if (member_opts.die.depth != opts.die.depth + 1) {
                                // this is a child of the struct but not a direct member, so skip
                                // (i.e. in C++ this could be a method on a class)
                                continue :done;
                            },
                            else => if (member_opts.die.depth <= opts.die.depth) break :done else continue :done,
                        }

                        try members.append(.{
                            .name = try parseAndCacheString(&member_opts, .DW_AT_name, str_cache),
                            .offset_bytes = try optionalAttribute(&member_opts, u32, .DW_AT_data_member_location) orelse 0,
                            .data_type = undefined, // will be assigned later
                        });

                        const type_offset = to: {
                            if (language == .Go) {
                                if (try optionalAttributeWithForm(&member_opts, Offset, .DW_AT_go_kind)) |go_kind| {
                                    break :to go_kind;
                                }
                            }

                            break :to try requiredAttributeWithForm(&member_opts, Offset, .DW_AT_type);
                        };

                        try delayed_refs.struct_member_types.append(cu.opts.scratch, .{
                            .is_global_offset = type_offset.isGlobalOffset(),
                            .type_offset = type_offset.data,
                            .struct_ndx = delayed_refs.struct_members.items.len,
                            .member_ndx = member_ndx,
                        });
                        member_ndx += 1;

                        assert(ndx <= max_members - 1);
                    }

                    const struct_type_ndx = data_types.items.len;
                    try delayed_refs.struct_members.append(cu.opts.scratch, .{
                        .struct_ndx = types.TypeNdx.from(struct_type_ndx),
                        .members = members,
                    });

                    try delayed_refs.addDataType(cu, opts.die, data_types);
                    try data_types.append(.{
                        .size_bytes = try optionalAttribute(&opts, u32, .DW_AT_byte_size) orelse 0,
                        .name = try parseAndCacheString(&opts, .DW_AT_name, str_cache),
                        .form = switch (opts.die.tag) {
                            .DW_TAG_structure_type, .DW_TAG_class_type => types.DataTypeForm{
                                .@"struct" = types.StructType{
                                    .members = undefined, // will be assigned later
                                },
                            },
                            .DW_TAG_union_type => types.DataTypeForm{
                                .@"union" = types.UnionType{
                                    .members = undefined, // will be assigned later
                                },
                            },
                            else => unreachable,
                        },
                    });
                },

                .DW_TAG_enumeration_type => {
                    const original_die_ndx = die_ndx;
                    defer die_ndx = original_die_ndx;

                    var values = ArrayList(types.EnumValue).init(cu.opts.scratch);

                    const max_values = pow(usize, 2, 12);
                    for (0..max_values) |value_ndx| {
                        die_ndx += 1;
                        if (die_ndx >= dies.len) break; // we've hit the end of the DIE list

                        const value_opts = AttributeParseOpts{ .cu = cu, .die = &dies[die_ndx] };
                        if (value_opts.die.tag != .DW_TAG_enumerator) break;

                        try values.append(.{
                            .name = try parseAndCacheString(&value_opts, .DW_AT_name, str_cache),
                            .value = types.EnumInstanceValue.from(
                                try requiredAttribute(&value_opts, i128, .DW_AT_const_value),
                            ),
                        });

                        assert(value_ndx <= max_values - 1);
                    }

                    try delayed_refs.addDataType(cu, opts.die, data_types);
                    try data_types.append(.{
                        .size_bytes = try optionalAttribute(&opts, u32, .DW_AT_byte_size) orelse 0,
                        .name = try parseAndCacheString(&opts, .DW_AT_name, str_cache),
                        .form = types.DataTypeForm{
                            .@"enum" = types.EnumType{
                                .values = try values.toOwnedSlice(),
                            },
                        },
                    });
                },

                else => {},
            }
        }
    }

    {
        const z1 = trace.zoneN(@src(), "assign function variables");
        defer z1.end();

        assert(functions.items.len == function_variables.items.len);
        for (functions.items, 0..) |*f, ndx| {
            f.variables = try function_variables.items[ndx].toOwnedSlice();
        }
    }

    return PartiallyReadyCompileUnit{
        .strings = str_cache,
        .delayed_refs = delayed_refs,
        .cu = types.CompileUnit{
            .address_size = cu.header.addr_size,
            .language = language,
            .ranges = ranges,
            .sources = sources,
            .variables = try variables.toOwnedSlice(),
            .functions = .{
                .functions = try functions.toOwnedSlice(),
                .ranges = try function_ranges.toOwnedSlice(),
            },
        },
    };
}

const DataTypeID = types.NumericType(u64);

/// Hashes `cu_ndx:name`, i.e. `FFFFFFFFFFFFFFFF:FFFFFFFFFFFFFFFF` so we can
/// uniquely track data types across multiple compile units
fn dataTypeID(cu_ndx: usize, name: strings.Hash) DataTypeID {
    var buf = [_]u8{0} ** 33;
    const str = fmt.bufPrint(&buf, "{x:0>16}:{x:0>16}", .{ cu_ndx, name }) catch unreachable;
    const hash = std.hash.Fnv1a_64.hash(str);
    return DataTypeID.from(hash);
}

pub const AttributeParseOpts = struct {
    cu: *const info.CompileUnit,
    die: *const info.DIE,
};

fn AttributeWithForm(comptime T: type) type {
    return struct {
        data: T,
        class: abbrev.FormClass,

        fn init(data: T, class: abbrev.FormClass) @This() {
            return .{
                .data = data,
                .class = class,
            };
        }

        fn isGlobalOffset(self: @This()) bool {
            return self.class == .global_reference;
        }
    };
}

pub fn requiredAttributeWithForm(
    opts: *const AttributeParseOpts,
    comptime T: type,
    name: consts.AttributeName,
) ParseError!AttributeWithForm(T) {
    if (try optionalAttributeWithForm(opts, T, name)) |v| return v;

    log.errf("required attribute {s} of type {any} not found on DIE of type {s} with offset 0x{x} in compile unit at offset 0x{x}", .{
        @tagName(name),
        T,
        @tagName(opts.die.tag),
        opts.die.offset,
        opts.cu.info_offset,
    });
    return error.InvalidDWARFInfo;
}

pub fn requiredAttribute(
    opts: *const AttributeParseOpts,
    comptime T: type,
    name: consts.AttributeName,
) ParseError!T {
    const res = try requiredAttributeWithForm(opts, T, name);
    return res.data;
}

/// Only strings allocate; all numeric values are just returned on the stack. If a string
/// is allocated, the caller owns returned memory.
pub fn optionalAttribute(
    opts: *const AttributeParseOpts,
    comptime T: type,
    name: consts.AttributeName,
) ParseError!?T {
    if (try optionalAttributeWithForm(opts, T, name)) |res| {
        return res.data;
    }
    return null;
}

pub fn optionalAttributeWithForm(
    opts: *const AttributeParseOpts,
    comptime T: type,
    name: consts.AttributeName,
) ParseError!?AttributeWithForm(T) {
    const z = trace.zone(@src());
    defer z.end();

    for (opts.die.specs) |*spec| {
        if (name != spec.name) continue;

        if (T == []const u8) {
            switch (spec.form) {
                .string => |str| {
                    const val = try str.parse(opts.cu, spec.offset);

                    const dst = try opts.cu.opts.scratch.alloc(u8, val.len);
                    errdefer opts.cu.opts.scratch.free(dst);

                    @memcpy(dst, val);
                    return AttributeWithForm(T).init(dst, spec.class);
                },

                // @NOTE (jrc): Some form types (i.e. DW_AT_location) can
                // be stored in one of many classes
                else => return null,
            }
        }

        switch (@typeInfo(T)) {
            .@"enum" => |e| {
                const val = try spec.parseNumeric(e.tag_type, opts.cu);
                const data = safe.enumFromInt(T, val) catch return error.InvalidDWARFInfo;
                return AttributeWithForm(T).init(data, spec.class);
            },

            .int => {
                const data = try spec.parseNumeric(T, opts.cu);
                return AttributeWithForm(T).init(data, spec.class);
            },

            // see above note about form types
            else => return null,
        }
    }

    return null;
}

pub fn parseAndCacheString(
    opts: *const AttributeParseOpts,
    name: consts.AttributeName,
    cache: *strings.Cache,
) ParseError!strings.Hash {
    const z = trace.zone(@src());
    defer z.end();

    const str = try optionalAttribute(opts, String, name) orelse "";
    return try cache.add(str);
}

pub fn getForm(
    opts: *const AttributeParseOpts,
    name: consts.AttributeName,
) ?abbrev.FormValue {
    for (opts.die.specs) |spec| {
        if (name == spec.name) return spec;
    }

    return null;
}

fn parseTypeEncoding(opts: *const AttributeParseOpts) ParseError!types.PrimitiveTypeEncoding {
    const enc = try requiredAttribute(opts, consts.AttributeEncoding, .DW_AT_encoding);
    return switch (enc) {
        .DW_ATE_boolean => .boolean,
        .DW_ATE_address => .unsigned,
        .DW_ATE_signed => .signed,
        .DW_ATE_signed_char => .signed,
        .DW_ATE_unsigned => .unsigned,
        .DW_ATE_unsigned_char => .unsigned,
        .DW_ATE_ASCII => .string,
        .DW_ATE_UCS => .string,
        .DW_ATE_UTF => .string,
        .DW_ATE_signed_fixed => .signed,
        .DW_ATE_unsigned_fixed => .unsigned,
        .DW_ATE_float => .float,
        .DW_ATE_complex_float => .complex,
        .DW_ATE_imaginary_float => .complex,
        .DW_ATE_decimal_float => .float,
        .DW_ATE_packed_decimal => .float,
        .DW_ATE_numeric_string => .string,
        .DW_ATE_edited => .string,
    };
}

fn parseSourceLoc(opts: *const AttributeParseOpts) ParseError!?types.SourceLocation {
    const file = try optionalAttribute(opts, usize, .DW_AT_decl_file) orelse return null;

    // look up the hash of the source file's absolute path
    const hash = blk: {
        if (file >= opts.cu.source_abs_path_hashes.items.len) {
            log.errf("source location hash file ndx out of range at offset 0x{x} (len {d}, got {d})", .{
                opts.die.offset,
                opts.cu.source_abs_path_hashes.items.len,
                file,
            });
            return error.InvalidDWARFInfo;
        }
        break :blk opts.cu.source_abs_path_hashes.items[file];
    };

    // not all DIEs that can emit a DW_AT_decl_line do, so skip if no line is found
    const decl_line = try optionalAttribute(opts, usize, .DW_AT_decl_line) orelse return null;

    // compilers frequently don't emit column info
    const decl_column: ?usize = blk: {
        const col = try optionalAttribute(opts, u8, .DW_AT_decl_column);
        if (col) |c| break :blk @intCast(c);
        break :blk null;
    };

    return types.SourceLocation{
        .file_hash = hash,
        .line = types.SourceLine.from(decl_line),
        .column = decl_column,
    };
}

fn parseLocationExpression(opts: *const AttributeParseOpts, str_cache: *strings.Cache) ParseError!?strings.Hash {
    if (getForm(opts, .DW_AT_location)) |form| {
        const expr = switch (form.class) {
            .block, .exprloc => try optionalAttribute(opts, []const u8, .DW_AT_location) orelse "",

            // @TODO (jrc): parse these from the appropriate section
            .loclist, .loclistptr => return null,

            else => {
                log.errf("invalid class for DW_AT_location at DIE offset 0x{x} in compile unit at offset 0x{x}: {s}", .{
                    opts.die.offset,
                    opts.cu.info_offset,
                    @tagName(form.class),
                });
                return error.InvalidDWARFInfo;
            },
        };

        return try str_cache.add(expr);
    }

    return null;
}
