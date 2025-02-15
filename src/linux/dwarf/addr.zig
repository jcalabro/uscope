//! Contains code for safely parsing the .debug_addr section of DWARF binaries

const std = @import("std");
const Allocator = std.mem.Allocator;
const ArrayList = std.ArrayList;
const assert = std.debug.assert;
const t = std.testing;

const dwarf = @import("../dwarf.zig");
const file = @import("../../file.zig");
const info = @import("info.zig");
const logging = @import("../../logging.zig");
const Reader = @import("../../Reader.zig");

const trace = @import("../../trace.zig");

const log = logging.Logger.init(logging.Region.Symbols);

/// Reads a single .debug_addr table and increments the passed `debug_addr_offset` by the
/// number of bytes that were read. These tables were introduced in DWARF v5.
/// @SEE: V5TABLES for more info because the two are very similar.
///
/// Allocated memory exists in the scratch arena.
pub fn parse(cu: *const info.CompileUnit, debug_addr_offset: *usize) dwarf.ParseError![]usize {
    const z = trace.zoneN(@src(), "parse .debug_addr");
    defer z.end();

    // .debug_addr was introduced in DWARF v5
    if (cu.header.version.isLessThan(.five) or
        cu.opts.sections.addr.contents.len == 0)
    {
        return &.{};
    }

    // this reader is only used to read the length field
    var r: Reader = undefined;
    r.init(cu.opts.sections.addr.contents[debug_addr_offset.*..]);

    // read the length of just this table and create a sub-reader
    const table_len = try dwarf.readInitialLength(&r);
    const start = debug_addr_offset.* + r.offset();
    const end = start + table_len;

    var table_r: Reader = undefined;
    table_r.init(cu.opts.sections.addr.contents[start..end]);

    // version
    const version = try dwarf.read(&table_r, u16);
    if (version != 5) {
        log.errf("invalid .debug_addr table version: {d}", .{version});
        return error.InvalidDWARFInfo;
    }

    const addr_size = try dwarf.read(&table_r, u8);
    const segment_selector_size = try dwarf.read(&table_r, u8);
    if (segment_selector_size != 0) {
        log.errf("non-zero .debug_addr segment selector sizes are not yet supported (got {d})", .{
            segment_selector_size,
        });
        return error.InvalidDWARFInfo;
    }

    var addrs = ArrayList(usize).init(cu.opts.scratch);
    errdefer addrs.deinit();

    while (!table_r.atEOF()) {
        const addr = switch (addr_size) {
            4 => try dwarf.read(&table_r, u32),
            8 => try dwarf.read(&table_r, u64),
            else => {
                log.errf("unsupported .debug_addr address size: {d}", .{addr_size});
                return error.InvalidDWARFInfo;
            },
        };

        try addrs.append(addr);
    }

    // advance the offset tracker
    debug_addr_offset.* += r.offset() + table_r.offset();

    return try addrs.toOwnedSlice();
}

test "parse cmulticu .debug_addr tables" {
    //
    // Expected values were taken from `readelf --debug-dump=addr`
    //

    var arena = std.heap.ArenaAllocator.init(t.allocator);
    defer arena.deinit();
    const scratch = arena.allocator();

    const sections = try scratch.create(dwarf.Sections);
    sections.addr = .{
        .addr = 0,
        .contents = info.getEmbeddedFile("linux_x86-64_cmulticu_out_addr"),
    };

    const opts = dwarf.ParseOpts{
        .scratch = scratch,
        .sections = sections,
        .file_cache = try file.Cache.init(scratch),
    };

    // stich up the CU header manually for this unit test based on real-world values from clang
    var cu = try scratch.create(info.CompileUnit);
    cu.opts = &opts;
    cu.header.is_32_bit = true;
    cu.header.version = .five;

    var addr_offset: usize = 0;

    {
        // read the first table
        const expected = [_]dwarf.Offset{ 0x402004, 0x401130 };

        const table = try parse(cu, &addr_offset);
        try t.expectEqualSlices(dwarf.Offset, &expected, table);
    }

    {
        // read the second table
        const expected = [_]dwarf.Offset{
            0x40201e,
            0x40202b,
            0x40203b,
            0x401160,
        };

        const table = try parse(cu, &addr_offset);
        try t.expectEqualSlices(dwarf.Offset, &expected, table);
    }

    // a third read would be invalid because there are only two talbes
    try t.expectError(error.InvalidDWARFInfo, parse(cu, &addr_offset));
}
