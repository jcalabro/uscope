//! Contains code for safely parsing DWARF debug symbols from binaries

const std = @import("std");
const builtin = @import("builtin");
const Allocator = mem.Allocator;
const ArenaAllocator = heap.ArenaAllocator;
const ArrayList = std.ArrayList;
const assert = std.debug.assert;
const AutoHashMap = std.AutoHashMap;
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

/// Parses the DWARF symbols from the given binary in to a platform-agnostic type. `perm_alloc` must be
/// a ThreadSafeAllocator under the hood.
pub fn parse(perm_alloc: Allocator, opts: *const ParseOpts, target: *types.Target) ParseError!void {
    const z = trace.zoneN(@src(), "parse dwarf");
    defer z.end();

    assert(opts.sections.info.contents.len > 0);
    assert(opts.sections.abbrev.contents.len > 0);
    assert(opts.sections.line.contents.len > 0);

    //
    // @SEARCH: MULTITHREADED_DWARF
    //
    // We spin up N worker threads to parse and map (most) debug info per compile unit
    // because the tasks are embarassingly parallel and we've observed sizable performance
    // gains when loading large real-world projects.
    //
    // The main thread parses the compile unit header and allocates the resulting memory
    // that is required, then passes the CU to a worker via a thread safe queue. The worker
    // picks it up, parses DWARF, and maps it to our x-platform types.
    //
    // We use a lot of brand-new, short lived memory arenas here because using a shared
    // ThreadSafeAllocator results in a TON of lock contention and it's super slow as a result.
    // We do all our work on thread-unsafe allocators, then at the end take a lock and map it
    // back to the main perm allocator.
    //

    // detach from the main thread's allocator for some of the allocations in this function
    var parse_alloc = MainAllocator.init();
    defer parse_alloc.deinit();

    // the data structures used to orchestrate multi-threaded parsing get their own
    // thread-safe scratch arena since we are using this memory from multiple threads
    const parse_scratch_arena = try parse_alloc.allocator().create(ArenaAllocator);
    parse_scratch_arena.* = ArenaAllocator.init(parse_alloc.allocator());
    defer parse_scratch_arena.deinit();

    const parse_scratch_tsa = try parse_alloc.allocator().create(ThreadSafeAllocator);
    parse_scratch_tsa.* = .{ .child_allocator = parse_scratch_arena.allocator() };
    const parse_scratch = parse_scratch_tsa.allocator();

    const req_queue = try opts.scratch.create(Queue(?CompileUnitRequest));
    req_queue.* = Queue(?CompileUnitRequest).init(parse_scratch_tsa, .{
        .timeout_ns = 100 * std.time.ns_per_ms,
    });
    defer req_queue.deinit();

    const err_queue = try parse_scratch.create(Queue(ParseError));
    err_queue.* = Queue(ParseError).init(parse_scratch_tsa, .{});
    defer err_queue.deinit();

    // accumulate responses in this array (must lock the mutex)
    var cus = try parse_scratch.create(ArrayList(types.CompileUnit));
    cus.* = ArrayList(types.CompileUnit).init(perm_alloc);
    const cus_mu = try parse_scratch.create(Thread.Mutex);
    cus_mu.* = .{};

    // set a limit on the number of threads because some machines out there are truly giant
    const num_threads = @max(32, Thread.getCpuCount() catch 4);

    // start worker threads
    var threads = try ArrayList(Thread).initCapacity(opts.scratch, num_threads);
    for (0..num_threads) |_| {
        const thread = try Thread.spawn(.{}, parseAndMapCompileUnits, .{
            perm_alloc,
            req_queue,
            err_queue,
            cus,
            target.strings,
            cus_mu,
        });
        safe.setThreadName(thread, "parseAndMapCompileUnit");
        threads.appendAssumeCapacity(thread);
    }

    const abbrev_tables = try abbrev.parse(opts);

    var offset: usize = 0;
    var offsets = TableOffsets{};

    // parse each CU header and send them to the worker threads
    const max = pow(usize, 2, 32);
    for (0..max) |cu_ndx| {
        if (offset >= opts.sections.info.contents.len) {
            // send poison pills to tell all workers to shutdown
            for (0..num_threads) |_| try req_queue.put(null);
            break;
        }

        const dwarf_cu = blk: {
            cus_mu.lock();
            defer cus_mu.unlock();

            _ = try cus.addOne();
            break :blk try info.CompileUnit.create(opts, offset);
        };

        // lock is needed because we're sharing the permanent allocator
        cus_mu.lock();
        defer cus_mu.unlock();

        try dwarf_cu.parseHeader(abbrev_tables, &offsets);
        if (dwarf_cu.header.addr_size.bytes() > target.addr_size.bytes()) {
            target.addr_size = dwarf_cu.header.addr_size;
        }

        try req_queue.put(.{ .ndx = cus.items.len - 1, .dwarf_cu = dwarf_cu });
        offset += dwarf_cu.header.total_len;

        assert(cu_ndx <= max - 1);
    }

    for (threads.items) |thread| thread.join();

    // check for errors
    if (err_queue.getOrNull()) |err| return err;

    target.compile_units = try cus.toOwnedSlice();
    target.unwinder = .{ .cies = try frame.loadTable(perm_alloc, opts) };
}

/// Sent from the main thread to the worker thread
const CompileUnitRequest = struct {
    ndx: usize,
    dwarf_cu: *info.CompileUnit,
};

fn parseAndMapCompileUnits(
    main_thread_perm_alloc: Allocator, // the allocator in use by the main debugger.zig thread
    cu_req_queue: *Queue(?CompileUnitRequest),
    err_queue: *Queue(ParseError),
    compile_units: *ArrayList(types.CompileUnit),
    all_strings: *strings.Cache,
    compile_units_mu: *Thread.Mutex,
) void {
    const z = trace.zone(@src());
    defer z.end();

    var thread_alloc = MainAllocator.init();
    defer thread_alloc.deinit();

    var scratch = ArenaAllocator.init(thread_alloc.allocator());
    defer scratch.deinit();

    const max = pow(usize, 2, 16);
    for (0..max) |cu_ndx| {
        if (cu_req_queue.get() catch continue) |*req| {
            const opts = ParseOpts{
                .scratch = scratch.allocator(),
                .sections = req.dwarf_cu.opts.sections,
            };
            req.dwarf_cu.opts = &opts;

            parseAndMapOneCompileUnit(
                main_thread_perm_alloc,
                req,
                compile_units,
                all_strings,
                compile_units_mu,
            ) catch |err| {
                err_queue.put(err) catch |e| {
                    log.errf("unable to add to dwarf parsing error queue: {!} (original error: {!})", .{ e, err });
                };
            };
        } else {
            // we're done
            break;
        }

        assert(cu_ndx <= max - 1);
    }
}

fn parseAndMapOneCompileUnit(
    main_thread_perm_alloc: Allocator,
    req: *const CompileUnitRequest,
    compile_units: *ArrayList(types.CompileUnit),
    all_strings: *strings.Cache,
    compile_units_mu: *Thread.Mutex,
) ParseError!void {
    const z = trace.zone(@src());
    defer z.end();

    const dies = try req.dwarf_cu.parseDIEs();
    const res = try mapDWARFToTarget(req.dwarf_cu, dies);

    {
        // copy the types.CompileUnit and string table to the main thread's permanent allocator
        compile_units_mu.lock();
        defer compile_units_mu.unlock();

        try compile_units.items[req.ndx].copyFrom(main_thread_perm_alloc, res.cu);

        // append to the global string table (no need to take a lock
        // on Cache.mu because we're already locked)
        try all_strings.map.ensureTotalCapacity(
            all_strings.alloc,
            all_strings.map.size + res.strings.map.size,
        );
        var str_it = res.strings.map.iterator();
        while (str_it.next()) |str| {
            all_strings.map.putAssumeCapacity(
                str.key_ptr.*,
                try strings.clone(all_strings.alloc, str.value_ptr.*),
            );
        }
    }
}

const CompileUnitWithStrings = struct {
    cu: types.CompileUnit,
    strings: *strings.Cache,
};

/// Converts the info.CompileUnit to a types.CompileUnit. All memory is allocated in the thread's
/// scratch allocator, so the returned type needs to be copied back to the main thread's perm alloc.
fn mapDWARFToTarget(cu: *info.CompileUnit, dies: []const info.DIE) ParseError!CompileUnitWithStrings {
    const z = trace.zone(@src());
    defer z.end();

    const str_cache = try strings.Cache.init(cu.opts.scratch);

    var language = types.Language.Unsupported;
    var ranges: []types.AddressRange = &.{};
    var sources: []types.SourceFile = &.{};

    var data_types = ArrayList(types.DataType).init(cu.opts.scratch);
    var variables = ArrayList(types.Variable).init(cu.opts.scratch);
    var function_variables = ArrayList(ArrayList(types.VariableNdx)).init(cu.opts.scratch);

    var functions = ArrayList(types.Function).init(cu.opts.scratch);
    var function_ranges = ArrayList(types.CompileUnit.Functions.Range).init(cu.opts.scratch);

    // we accumulate DW_AT_type (an offset) mapped to the index in the variables array in
    // this temporary buffer so we can stitch type information together after the first pass
    const VariableTypeEntry = struct {
        type_offset: Offset,
        variable_ndx: types.VariableNdx,
    };
    var variable_types = ArrayList(VariableTypeEntry).init(cu.opts.scratch);
    var data_type_map = AutoHashMap(Offset, types.TypeNdx).init(cu.opts.scratch);

    // do the same thing for array types that we do for variables
    // (i.e. for []u8, we want to set its element type to u8)
    var array_types = ArrayList(VariableTypeEntry).init(cu.opts.scratch);

    // do the same thing for pointer types
    // (i.e. for *u8, we want to set its reference type to u8)
    const PointerTypeEntry = struct {
        type_offset: ?Offset,
        variable_ndx: types.VariableNdx,
    };
    var pointer_types = ArrayList(PointerTypeEntry).init(cu.opts.scratch);

    // do the same for const types
    var const_types = ArrayList(VariableTypeEntry).init(cu.opts.scratch);

    // do the same thing for typedefs
    var typedef_types = ArrayList(VariableTypeEntry).init(cu.opts.scratch);

    // accumulate type info for struct members
    const StructMemberListEntry = struct {
        struct_ndx: types.TypeNdx,
        members: ArrayList(types.MemberType),
    };
    var struct_members = ArrayList(StructMemberListEntry).init(cu.opts.scratch);
    const StructMemberTypeEntry = struct {
        /// the offset of the type of the member
        type_offset: Offset,
        /// the offset to the struct in the struct_members list
        struct_ndx: usize,
        /// the index of the member within the struct's member list
        member_ndx: usize,
    };
    var struct_member_types = ArrayList(StructMemberTypeEntry).init(cu.opts.scratch);

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

                    var range_opts = aranges.ParseOpts{
                        .opts = &opts,
                        .sources = sources,
                        .func_statements = null,
                    };
                    ranges = try aranges.parse(&range_opts);
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

                .DW_TAG_subroutine_type => {
                    // store this function declaration as a DataType so we can use it for function pointers
                    try data_type_map.put(opts.die.offset, types.TypeNdx.from(data_types.items.len));
                    try data_types.append(.{
                        .size_bytes = opts.cu.header.addr_size.bytes(),
                        .name = try parseAndCacheString(&opts, .DW_AT_name, str_cache),
                        .form = types.DataTypeForm{ .function = types.FunctionType{} },
                    });
                },

                .DW_TAG_variable, .DW_TAG_formal_parameter => {
                    if (try optionalAttribute(&opts, Offset, .DW_AT_type)) |type_offset| {
                        try variable_types.append(.{
                            .type_offset = type_offset,
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
                        .name = try parseAndCacheString(&opts, .DW_AT_name, str_cache),
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
                    try data_type_map.put(opts.die.offset, types.TypeNdx.from(data_types.items.len));
                    try data_types.append(.{
                        .size_bytes = 0,
                        .name = try parseAndCacheString(&opts, .DW_AT_name, str_cache),
                        .form = types.DataTypeForm{ .unknown = types.UnknownType{} },
                    });
                },

                .DW_TAG_base_type => {
                    try data_type_map.put(opts.die.offset, types.TypeNdx.from(data_types.items.len));
                    try data_types.append(.{
                        .size_bytes = try requiredAttribute(&opts, u32, .DW_AT_byte_size),
                        .name = try parseAndCacheString(&opts, .DW_AT_name, str_cache),
                        .form = types.DataTypeForm{ .primitive = types.PrimitiveType{
                            .encoding = try parseTypeEncoding(&opts),
                        } },
                    });
                },

                .DW_TAG_const_type => {
                    if (try optionalAttribute(&opts, Offset, .DW_AT_type)) |type_offset| {
                        try const_types.append(.{
                            .type_offset = type_offset,
                            .variable_ndx = types.VariableNdx.from(data_types.items.len),
                        });
                    }

                    try data_type_map.put(opts.die.offset, types.TypeNdx.from(data_types.items.len));
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
                    if (try optionalAttribute(&opts, Offset, .DW_AT_type)) |type_offset| {
                        try typedef_types.append(.{
                            .type_offset = type_offset,
                            .variable_ndx = types.VariableNdx.from(data_types.items.len),
                        });
                    }

                    try data_type_map.put(opts.die.offset, types.TypeNdx.from(data_types.items.len));
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
                    try pointer_types.append(.{
                        .type_offset = try optionalAttribute(&opts, Offset, .DW_AT_type),
                        .variable_ndx = types.VariableNdx.from(data_types.items.len),
                    });

                    const num_bytes = cu.header.addr_size.bytes();

                    try data_type_map.put(opts.die.offset, types.TypeNdx.from(data_types.items.len));
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
                    if (try optionalAttribute(&opts, Offset, .DW_AT_type)) |type_offset| {
                        try array_types.append(.{
                            .type_offset = type_offset,
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

                    try data_type_map.put(opts.die.offset, types.TypeNdx.from(data_types.items.len));
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
                        try struct_member_types.append(.{
                            .type_offset = try requiredAttribute(&member_opts, Offset, .DW_AT_type),
                            .struct_ndx = struct_members.items.len,
                            .member_ndx = member_ndx,
                        });
                        member_ndx += 1;

                        assert(ndx <= max_members - 1);
                    }

                    const struct_type_ndx = data_types.items.len;
                    try struct_members.append(.{
                        .struct_ndx = types.TypeNdx.from(struct_type_ndx),
                        .members = members,
                    });

                    try data_type_map.put(opts.die.offset, types.TypeNdx.from(struct_type_ndx));
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

                    try data_type_map.put(opts.die.offset, types.TypeNdx.from(data_types.items.len));
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
        const z1 = trace.zoneN(@src(), "assign const types");
        defer z1.end();

        for (const_types.items) |const_type| {
            const data_type_ndx = dt: {
                if (data_type_map.get(const_type.type_offset)) |dt_ndx| {
                    if (dt_ndx.int() < data_types.items.len) {
                        break :dt dt_ndx;
                    }
                }

                log.errf("unable to find data type for const with offset 0x{x}", .{
                    const_type.type_offset,
                });
                return error.InvalidDWARFInfo;
            };

            const constant = &data_types.items[const_type.variable_ndx.int()];
            constant.*.form.constant.data_type = data_type_ndx;
        }
    }

    {
        const z1 = trace.zoneN(@src(), "assign typedef types");
        defer z1.end();

        for (typedef_types.items) |typedef_type| {
            const data_type_ndx = dt: {
                if (data_type_map.get(typedef_type.type_offset)) |dt_ndx| {
                    if (dt_ndx.int() < data_types.items.len) {
                        break :dt dt_ndx;
                    }
                }

                log.errf("unable to find data type for typedef with offset 0x{x}", .{
                    typedef_type.type_offset,
                });
                return error.InvalidDWARFInfo;
            };

            const ptr = &data_types.items[typedef_type.variable_ndx.int()];
            ptr.*.form.typedef.data_type = data_type_ndx;
        }
    }

    {
        const z1 = trace.zoneN(@src(), "assign struct member types");
        defer z1.end();

        // first, assign all types to members
        for (struct_member_types.items) |member_type| {
            const data_type_ndx = dt: {
                if (data_type_map.get(member_type.type_offset)) |dt_ndx| {
                    if (dt_ndx.int() < data_types.items.len) {
                        break :dt dt_ndx;
                    }
                }

                log.errf("unable to find data type for member of struct with offset 0x{x}", .{
                    member_type.type_offset,
                });
                return error.InvalidDWARFInfo;
            };

            struct_members.items[member_type.struct_ndx]
                .members.items[member_type.member_ndx].data_type = data_type_ndx;
        }

        // then, assign all member arrays to structs
        for (struct_members.items) |*members| {
            const data_type = &data_types.items[members.struct_ndx.int()];
            switch (data_type.*.form) {
                .@"struct" => |*s| s.members = try members.members.toOwnedSlice(),
                .@"union" => |*u| u.members = try members.members.toOwnedSlice(),
                else => unreachable,
            }
        }
    }

    {
        const z1 = trace.zoneN(@src(), "assign pointer types");
        defer z1.end();

        for (pointer_types.items) |ptr_type| {
            if (ptr_type.type_offset == null) continue;

            const data_type_ndx = dt: {
                if (data_type_map.get(ptr_type.type_offset.?)) |dt_ndx| {
                    if (dt_ndx.int() < data_types.items.len) {
                        break :dt dt_ndx;
                    }
                }

                log.errf("unable to find data type for pointer with offset 0x{x}", .{
                    ptr_type.type_offset.?,
                });
                return error.InvalidDWARFInfo;
            };

            const ptr = &data_types.items[ptr_type.variable_ndx.int()];
            ptr.*.form.pointer.data_type = data_type_ndx;

            // pointer name may or may not already be set at this time
            const ptr_name = str_cache.get(ptr.name);
            if (ptr_name == null or ptr_name.?.len == 0) {
                const data_type = data_types.items[data_type_ndx.int()];
                const item_type_name = str_cache.get(data_type.name) orelse types.Unknown;
                const type_name = try types.PointerType.nameFromItemType(cu.opts.scratch, item_type_name);
                ptr.*.name = try str_cache.add(type_name);
            }
        }
    }

    {
        const z1 = trace.zoneN(@src(), "assign array types");
        defer z1.end();

        for (array_types.items) |arr_type| {
            const data_type_ndx = dt: {
                if (data_type_map.get(arr_type.type_offset)) |dt_ndx| {
                    if (dt_ndx.int() < data_types.items.len) {
                        break :dt dt_ndx;
                    }
                }

                log.errf("unable to find data type for array with offset 0x{x}", .{
                    arr_type.type_offset,
                });
                return error.InvalidDWARFInfo;
            };

            const data_type = data_types.items[data_type_ndx.int()];
            const item_type_name = str_cache.get(data_type.name) orelse types.Unknown;
            const type_name = try types.ArrayType.nameFromItemType(cu.opts.scratch, item_type_name);

            const arr_ptr = &data_types.items[arr_type.variable_ndx.int()];
            arr_ptr.*.form.array.element_type = data_type_ndx;
            arr_ptr.*.name = try str_cache.add(type_name);

            arr_ptr.*.size_bytes = 0;
            if (arr_ptr.form.array.len) |len| {
                arr_ptr.*.size_bytes = data_type.size_bytes * len;
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

    {
        const z1 = trace.zoneN(@src(), "assign variable types");
        defer z1.end();

        for (variable_types.items) |vt| {
            const data_type_ndx = dt: {
                if (data_type_map.get(vt.type_offset)) |dt_ndx| {
                    if (dt_ndx.int() < data_types.items.len) {
                        break :dt dt_ndx;
                    }
                }

                log.errf("unable to find data type for variable with offset 0x{x}", .{
                    vt.type_offset,
                });
                return error.InvalidDWARFInfo;
            };

            const var_ptr = &variables.items[vt.variable_ndx.int()];
            var_ptr.*.data_type = data_type_ndx;
        }
    }

    return .{
        .strings = str_cache,
        .cu = types.CompileUnit{
            .address_size = cu.header.addr_size,
            .language = language,
            .ranges = ranges,
            .sources = sources,
            .data_types = try data_types.toOwnedSlice(),
            .variables = try variables.toOwnedSlice(),
            .functions = .{
                .functions = try functions.toOwnedSlice(),
                .ranges = try function_ranges.toOwnedSlice(),
            },
        },
    };
}

pub const AttributeParseOpts = struct {
    cu: *const info.CompileUnit,
    die: *const info.DIE,
};

pub fn requiredAttribute(
    opts: *const AttributeParseOpts,
    comptime T: type,
    name: consts.AttributeName,
) ParseError!T {
    const z = trace.zone(@src());
    defer z.end();

    if (try optionalAttribute(opts, T, name)) |v| return v;

    log.errf("required attribute {s} of type {any} not found on DIE of type {s} with offset 0x{x} in compile unit at offset 0x{x}", .{
        @tagName(name),
        T,
        @tagName(opts.die.tag),
        opts.die.offset,
        opts.cu.info_offset,
    });
    return error.InvalidDWARFInfo;
}

/// Only strings allocate; all numeric values are just returned on the stack. If a string
/// is allocated, the caller owns returned memory.
pub fn optionalAttribute(
    opts: *const AttributeParseOpts,
    comptime T: type,
    name: consts.AttributeName,
) ParseError!?T {
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
                    return dst;
                },

                // @NOTE (jrc): Some form types (i.e. DW_AT_location) can
                // be stored in one of many classes
                else => return null,
            }
        }

        switch (@typeInfo(T)) {
            .@"enum" => |e| {
                const val = try spec.parseNumeric(e.tag_type, opts.cu);
                return safe.enumFromInt(T, val) catch return error.InvalidDWARFInfo;
            },

            .int => return try spec.parseNumeric(T, opts.cu),

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
