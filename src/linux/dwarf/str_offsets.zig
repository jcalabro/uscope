//! Contains code for safely parsing the .debug_str_offsets section of DWARF binaries

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

// @SEARCH: V5TABLES
/// Reads a single .debug_str_offsets table and increments the passed `debug_str_table_offset` by
/// the number of bytes that were read. These tables were introduced in DWARF v5. Each table has its
/// own header, and we need to keep track of the total offset we've scanned through in the section
/// as we're reading through compile units. This aspect of DWARF is pretty poorly designed.
///
/// Allocated memory exists in the scratch arena.
///
/// These tables are my livelihood!
pub fn parse(cu: *const info.CompileUnit, debug_str_offsets_offset: *usize) dwarf.ParseError![]dwarf.Offset {
    const z = trace.zoneN(@src(), "parse .debug_str_offsets");
    defer z.end();

    // .debug_str_offsets was introduced in DWARF v5
    if (cu.header.version.isLessThan(.five) or
        cu.opts.sections.str_offsets.contents.len == 0)
    {
        return &.{};
    }

    // this reader is only used to read the length field
    var r: Reader = undefined;
    r.init(cu.opts.sections.str_offsets.contents[debug_str_offsets_offset.*..]);

    // read the length of just this table and create a sub-reader
    const table_len = try dwarf.readInitialLength(&r);
    const start = debug_str_offsets_offset.* + r.offset();
    const end = start + table_len;

    var table_r: Reader = undefined;
    table_r.init(cu.opts.sections.str_offsets.contents[start..end]);

    // version
    const version = try dwarf.read(&table_r, u16);
    if (version != 5) {
        log.errf("invalid .debug_str_offsets table version: {d}", .{version});
        return error.InvalidDWARFInfo;
    }

    // padding
    _ = try dwarf.read(&table_r, u16);

    var offsets = ArrayList(dwarf.Offset).init(cu.opts.scratch);
    errdefer offsets.deinit();

    while (!table_r.atEOF()) {
        const offset = try cu.readOffset(&table_r);
        try offsets.append(offset);
    }

    // advance the offset tracker
    debug_str_offsets_offset.* += r.offset() + table_r.offset();

    return try offsets.toOwnedSlice();
}

test "parse cmulticu .debug_str_offsets tables" {
    //
    // Expected values were taken from `readelf --debug-dump=str-offsets` since there
    // appears to be a bug in `dwarfdump --print-str-offsets` where they're missing
    // the first two entries in the first table
    //

    var arena = std.heap.ArenaAllocator.init(t.allocator);
    defer arena.deinit();
    const scratch = arena.allocator();

    const sections = try scratch.create(dwarf.Sections);
    sections.str_offsets = .{
        .addr = 0,
        .contents = info.getEmbeddedFile("linux_x86-64_cmulticu_out_str_offsets"),
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

    var str_table_offset: usize = 0;

    {
        // read the first table
        const expected = [_]dwarf.Offset{
            0x00,
            0x15,
            0x1e,
            0x60,
            0x65,
            0x79,
            0x80,
            0x83,
        };

        const table = try parse(cu, &str_table_offset);
        try t.expectEqualSlices(dwarf.Offset, &expected, table);
    }

    {
        // read the second table
        const expected = [_]dwarf.Offset{
            0x00,
            0x87,
            0x1e,
            0x60,
            0x65,
            0x8e,
            0x93,
            0x97,
            0xa1,
            0xa7,
        };

        const table = try parse(cu, &str_table_offset);
        try t.expectEqualSlices(dwarf.Offset, &expected, table);
    }

    // a third read would be invalid because there are only two talbes
    try t.expectError(error.InvalidDWARFInfo, parse(cu, &str_table_offset));
}
