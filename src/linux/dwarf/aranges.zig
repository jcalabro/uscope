//! Contains code for safely parsing DWARF address ranges from binaries

const std = @import("std");
const assert = std.debug.assert;
const ArrayList = std.ArrayList;
const math = std.math;

const Address = types.Address;
const consts = @import("consts.zig");
const dwarf = @import("../dwarf.zig");
const info = @import("info.zig");
const line = @import("line.zig");
const logging = @import("../../logging.zig");
const Reader = @import("../../Reader.zig");
const trace = @import("../../trace.zig");
const types = @import("../../types.zig");

const log = logging.Logger.init(logging.Region.Symbols);

pub const ParseOpts = struct {
    opts: *const dwarf.AttributeParseOpts,
    sources: []types.SourceFile,
    func_statements: ?*ArrayList(types.SourceStatement),
};

/// Parses the list of address ranges contianed within the compilation unit DIE. Some DIEs only have a low_pc
/// and high_pc, others may have only a range list in a separate section, and yet others may contain both. Address
/// ranges do not need to be contiguous and are not ordered. Address ranges never overlap.
pub fn parse(
    parse_opts: *ParseOpts,
) dwarf.ParseError![]types.AddressRange {
    const z = trace.zone(@src());
    defer z.end();

    var addr_ranges = ArrayList(types.AddressRange).init(parse_opts.opts.cu.opts.scratch);

    const low_pc = try dwarf.optionalAttribute(parse_opts.opts, u64, .DW_AT_low_pc);
    if (low_pc) |low| {
        if (try dwarf.optionalAttribute(parse_opts.opts, u64, .DW_AT_high_pc)) |high_val| {
            const class = dwarf.getForm(parse_opts.opts, .DW_AT_high_pc).?.class;
            const high_pc = switch (class) {
                // if it's an address, just use it
                .address => high_val,

                // if it's a constant, add it to the low_pc
                .constant => low + high_val,

                else => {
                    log.errf("invalid DW_AT_high_pc class on compile unit at offset 0x{x}: {s}", .{
                        parse_opts.opts.cu.info_offset,
                        @tagName(class),
                    });
                    return error.InvalidDWARFInfo;
                },
            };

            try addRange(parse_opts, &addr_ranges, .{
                .low = Address.from(low),
                .high = Address.from(high_pc),
            });
        }
    }

    if (try dwarf.optionalAttribute(parse_opts.opts, u64, .DW_AT_ranges)) |offset| {
        // we've already checked that the attribute exists, to ? is safe
        const class = dwarf.getForm(parse_opts.opts, .DW_AT_ranges).?.class;

        var ranges_reader: Reader = undefined;
        ranges_reader.init(try class.contents(parse_opts.opts.cu, offset));

        const base_addr = blk: {
            // @NOTE (jrc): In one of the DWARF v3 drafts, there was a separate
            // attribute DW_AT_entry_pc which was since removed, but some versions
            // of gcc still use it, so apply that as the base address if it exists
            if (try dwarf.optionalAttribute(parse_opts.opts, u64, .DW_AT_entry_pc)) |b| break :blk b;

            // fall back to the low PC on this DIE, if any
            if (low_pc) |low| break :blk low;

            // it's likely the case that the first entry in the list is the base address,
            // which we will parse later
            break :blk 0;
        };

        if (parse_opts.opts.cu.header.version.isAtLeast(.five)) {
            try parseAddrRangesV5(parse_opts, &ranges_reader, base_addr, &addr_ranges);
        } else {
            try parseAddrRangesV2(parse_opts, &ranges_reader, base_addr, &addr_ranges);
        }
    }

    return addr_ranges.toOwnedSlice();
}

/// We need to use a relatively large max here because of Rust
const MAX_RANGELIST_ENTRIES = std.math.pow(usize, 2, 18);

fn parseAddrRangesV2(
    parse_opts: *ParseOpts,
    ranges_reader: *Reader,
    base_addr: u64,
    addr_ranges: *ArrayList(types.AddressRange),
) dwarf.ParseError!void {
    var base: u64 = base_addr;

    const largest: u64 = switch (parse_opts.opts.cu.header.addr_size) {
        .four => math.maxInt(u32),
        .eight => math.maxInt(u64),
    };

    for (0..MAX_RANGELIST_ENTRIES) |ndx| {
        const low_pc = try dwarf.readAddr(ranges_reader, parse_opts.opts.cu.header.addr_size);
        const high_pc = try dwarf.readAddr(ranges_reader, parse_opts.opts.cu.header.addr_size);

        // we've reached the end of list entry and we are done
        if (low_pc == 0 and high_pc == 0) break;

        // a low_pc value of the max int indicates that we should set the base address
        if (low_pc == largest) {
            base = high_pc;
            continue;
        }

        if (low_pc > high_pc) {
            log.errf("invalid addr range: low of 0x{x} is greater than high of 0x{x}", .{
                low_pc,
                high_pc,
            });
            return error.InvalidDWARFInfo;
        }

        if (low_pc == largest) {
            base = high_pc;
        } else {
            {
                const search_zone = trace.zone(@src());
                defer search_zone.end();

                try addRange(parse_opts, addr_ranges, .{
                    .low = Address.from(base + low_pc),
                    .high = Address.from(base + high_pc),
                });
            }
        }

        assert(ndx <= MAX_RANGELIST_ENTRIES - 1);
    }
}

fn parseAddrRangesV5(
    parse_opts: *ParseOpts,
    ranges_reader: *Reader,
    base_addr: u64,
    addr_ranges: *ArrayList(types.AddressRange),
) dwarf.ParseError!void {
    const cu_base_addr = try dwarf.optionalAttribute(parse_opts.opts, u64, .DW_AT_addr_base) orelse 0;

    var base: u64 = base_addr;
    for (0..MAX_RANGELIST_ENTRIES) |ndx| {
        const opcode = try dwarf.readEnum(ranges_reader, u8, consts.RangeListEntry);
        switch (opcode) {
            .DW_RLE_end_of_list => return,

            .DW_RLE_base_addressx => {
                const base_ndx = try dwarf.readULEB128(ranges_reader);
                base = try debugInfoAddr(parse_opts.opts.cu, cu_base_addr, base_ndx);
            },

            .DW_RLE_startx_endx => {
                const low_ndx = try dwarf.readULEB128(ranges_reader);
                const high_ndx = try dwarf.readULEB128(ranges_reader);

                const low = try debugInfoAddr(parse_opts.opts.cu, cu_base_addr, low_ndx);
                const high = try debugInfoAddr(parse_opts.opts.cu, cu_base_addr, high_ndx);

                try addRange(parse_opts, addr_ranges, .{
                    .low = Address.from(low),
                    .high = Address.from(high),
                });
            },

            .DW_RLE_startx_length => {
                const low_ndx = try dwarf.readULEB128(ranges_reader);
                const len = try dwarf.readULEB128(ranges_reader);

                const low = try debugInfoAddr(parse_opts.opts.cu, cu_base_addr, low_ndx);
                try addRange(parse_opts, addr_ranges, .{
                    .low = Address.from(low),
                    .high = Address.from(low + len),
                });
            },

            .DW_RLE_offset_pair => {
                const start = try dwarf.readULEB128(ranges_reader);
                const end = try dwarf.readULEB128(ranges_reader);

                try addRange(parse_opts, addr_ranges, .{
                    .low = Address.from(start + base),
                    .high = Address.from(end + base),
                });
            },

            .DW_RLE_base_address => {
                base = try dwarf.readAddr(ranges_reader, parse_opts.opts.cu.header.addr_size);
            },

            .DW_RLE_start_end => {
                const start = try dwarf.readAddr(ranges_reader, parse_opts.opts.cu.header.addr_size);
                const end = try dwarf.readAddr(ranges_reader, parse_opts.opts.cu.header.addr_size);

                try addRange(parse_opts, addr_ranges, .{
                    .low = Address.from(start),
                    .high = Address.from(end),
                });
            },

            .DW_RLE_start_length => {
                const start = try dwarf.readAddr(ranges_reader, parse_opts.opts.cu.header.addr_size);
                const len = try dwarf.readULEB128(ranges_reader);

                try addRange(parse_opts, addr_ranges, .{
                    .low = Address.from(start),
                    .high = Address.from(start + len),
                });
            },
        }

        assert(ndx <= MAX_RANGELIST_ENTRIES - 1);
    }
}

fn addRange(
    parse_opts: *ParseOpts,
    addr_ranges: *ArrayList(types.AddressRange),
    range: types.AddressRange,
) !void {
    if (parse_opts.func_statements != null) {
        // @SEARCH: FUNC_STMT_PERF
        // @PERFORMANCE (jrc): What can we do to trim down the number of source
        // statements we need to search through? This is crazy slow.
        for (parse_opts.sources) |src| {
            for (src.statements) |stmt| {
                if (!range.contains(stmt.breakpoint_addr)) continue;

                // this statement is contained within the body of this function
                try parse_opts.func_statements.?.append(stmt);
            }
        }
    }

    try addr_ranges.append(range);
}

/// Looks up an address in the .debug_addr section (only used with DWARF v5 and above)
fn debugInfoAddr(cu: *const info.CompileUnit, base_addr: u64, ndx: u64) dwarf.ParseError!u64 {
    const offset = ndx * cu.header.addr_size.bytes() + base_addr;

    var r: Reader = undefined;
    r.init(cu.opts.sections.addr.contents);
    r.seek(offset);

    return try dwarf.readAddr(&r, cu.header.addr_size);
}
