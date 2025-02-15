//! Contains code for safely parsing the .debug_info section of DWARF binaries

const std = @import("std");
const builtin = @import("builtin");
const Allocator = std.mem.Allocator;
const ArenaAllocator = std.heap.ArenaAllocator;
const ArrayList = std.ArrayList;
const ArrayListUnmanaged = std.ArrayListUnmanaged;
const assert = std.debug.assert;
const mem = std.mem;
const pow = std.math.pow;
const t = std.testing;

const abbrev = @import("abbrev.zig");
const consts = @import("consts.zig");
const dwarf = @import("../dwarf.zig");
const debug_addr = @import("addr.zig");
const file_util = @import("../../file.zig");
const logging = @import("../../logging.zig");
const Offset = dwarf.Offset;
const read = dwarf.read;
const Reader = @import("../../Reader.zig");
const safe = @import("../../safe.zig");
const str_offsets = @import("str_offsets.zig");
const trace = @import("../../trace.zig");
const types = @import("../../types.zig");

const log = logging.Logger.init(logging.Region.Symbols);

pub const CompileUnit = struct {
    const Self = @This();

    opts: *const dwarf.ParseOpts,
    abbrev_table: *const abbrev.Table = undefined,

    header: Header,

    str_offsets_table: []Offset = undefined,
    addr_table: []usize = undefined,

    info_r: *Reader,
    info_offset: usize,

    source_abs_path_hashes: ArrayListUnmanaged(file_util.Hash) = .{},

    pub fn create(
        opts: *const dwarf.ParseOpts,
        offset: usize,
    ) dwarf.ParseError!*Self {
        const self = try opts.scratch.create(Self);

        // peek at the first field in this CU to see how
        // long the reader's backing buffer should be
        const buf_len = blk: {
            var r: Reader = undefined;
            r.init(opts.sections.info.contents[offset..]);
            const len = try dwarf.readInitialLength(&r);
            break :blk r.offset() + len;
        };
        const info_start = offset;
        const info_end = offset + buf_len;
        const r = try Reader.create(opts.scratch, opts.sections.info.contents[info_start..info_end]);

        self.* = .{
            .opts = opts,
            .header = Header{},
            .info_r = r,
            .info_offset = offset,
        };

        return self;
    }

    /// readOffset reads 4 bytes in the .debug_info section if the CU is a 32 bit
    /// CU, and 64 if it's a 64 bit CU, then always returns that data as a u64
    pub fn readOffset(self: *const Self, r: *Reader) error{InvalidDWARFInfo}!Offset {
        if (!self.header.is_32_bit) return read(r, Offset);

        const val = try read(r, u32);
        return @intCast(val);
    }

    /// Reads an Offset form the .debug_info section (convenience wrapper)
    pub fn readInfoOffset(self: *Self) error{InvalidDWARFInfo}!Offset {
        return self.readOffset(self.info_r);
    }

    /// Finds and sets the appropriate .debug_abbrev table for this compile unit based
    /// on the compile unit header value
    fn setAbbrevTable(self: *Self, tables: []abbrev.Table) error{InvalidDWARFInfo}!void {
        for (tables) |*table| {
            if (table.offset == self.header.debug_abbrev_offset) {
                self.abbrev_table = table;
                return;
            }
        }

        return error.InvalidDWARFInfo;
    }

    /// Parses all header information for a given compile unit. Returned memory is allocated in the scratch arena.
    pub fn parseHeader(
        self: *Self,
        abbrev_tables: []abbrev.Table,
        offsets: *dwarf.TableOffsets,
    ) dwarf.ParseError!void {
        const z = trace.zoneN(@src(), "parse compile unit");
        defer z.end();

        try CompileUnit.Header.parse(self);
        try self.setAbbrevTable(abbrev_tables);

        self.str_offsets_table = try str_offsets.parse(self, &offsets.debug_str_offsets);
        self.addr_table = try debug_addr.parse(self, &offsets.debug_addr);
    }

    /// Parses all DIEs for a given compile unit. Returned memory is allocated in the scratch arena.
    pub fn parseDIEs(self: *Self) dwarf.ParseError![]DIE {
        const z = trace.zone(@src());
        defer z.end();

        var dies = ArrayList(DIE).init(self.opts.scratch);
        var die_tree = ArrayList(abbrev.Code).init(self.opts.scratch);

        const max = std.math.pow(usize, 2, 24);
        for (0..max) |die_ndx| {
            // sometimes, compilers don't specify exit abbrev codes
            if (self.info_r.atEOF()) break;

            const die_offset = self.info_r.offset();

            const abbrev_code = try dwarf.readULEB128(self.info_r);
            if (abbrev_code == 0) {
                // we're done
                if (die_tree.items.len <= 1) break;

                // this DIE has no more children
                _ = die_tree.popOrNull();
                continue;
            }

            const abbrev_decl = try self.abbrev_table.getDecl(abbrev_code);
            var specs = try ArrayList(abbrev.FormValue).initCapacity(
                self.opts.scratch,
                abbrev_decl.attrs.items.len,
            );

            for (abbrev_decl.attrs.items, 0..) |attr, attr_ndx| {
                _ = specs.appendAssumeCapacity(.{
                    .offset = undefined,
                    .name = attr.name,
                    .form = undefined,
                    .class = undefined,
                });
                try attr.chooseFormAndAdvanceBySize(&specs.items[attr_ndx], self);
            }

            const die = try dies.addOne();
            die.* = .{
                .offset = die_offset,
                .depth = die_tree.items.len,
                .tag = abbrev_decl.tag,
                .specs = try specs.toOwnedSlice(),
            };

            if (abbrev_decl.has_children) {
                try die_tree.append(abbrev_code);
            }

            assert(die_ndx < max - 1);
        }

        assert(self.info_r.offset() == self.header.total_len);

        return dies.toOwnedSlice();
    }

    const Header = struct {
        /// is_32_bit represents whether or not the debug info we're parsing is
        /// 32 or 64 bit DWARF. This is not the same as addr_size, which is the
        /// size of an address on the target architecture.
        is_32_bit: bool = undefined,

        /// The number of bytes taken by this compilation unit header
        /// in the .debug_info section (minus the number of bytes it takes
        /// to store the length itself, either 4 or 12)
        len: Offset = undefined,

        /// total_len is not a field in the compile unit header, but we use
        /// it to indicate the total number of bytes in the compile unit, which
        /// is equal to the len field plus the number of initial bytes.
        total_len: Offset = undefined,

        /// Version of the DWARF standard used for this CU. Supported values
        /// are 3, 4, and 5.
        version: dwarf.Version = undefined,

        /// (added in v5) The type of compilation unit (i.e. full, partial, skeleton, etc.)
        unit_type: consts.CompilationUnitHeaderType = .DW_UT_unknown,

        /// The offset in to the .debug_abbrev section where tags and attributes
        /// for the DIEs of this CU are located
        debug_abbrev_offset: Offset = undefined,

        /// The number of bytes that one address takes on the debugee's
        /// target architecture
        addr_size: types.AddressSize = undefined,

        /// Parses the metadata stored in the compile unit's header section
        fn parse(cu: *CompileUnit) dwarf.ParseError!void {
            const z = trace.zoneN(@src(), "parse compile unit header");
            defer z.end();

            assert(cu.info_r.offset() == 0);

            cu.header.len = try dwarf.readInitialLength(cu.info_r);
            cu.header.total_len = cu.header.len + cu.info_r.offset();
            cu.header.is_32_bit = cu.info_r.offset() == 4;

            const version = try read(cu.info_r, u16);
            cu.header.version = switch (version) {
                3 => .three,
                4 => .four,
                5 => .five,

                else => {
                    log.errf("invalid debug info version: {d}", .{version});
                    return error.InvalidDWARFVersion;
                },
            };

            if (cu.header.version.isAtLeast(.five)) {
                // unit_type is new in v5
                cu.header.unit_type = try read(cu.info_r, consts.CompilationUnitHeaderType);

                // @TODO (jrc): parse other CU types such as skeletons (v4/v5)
                switch (cu.header.unit_type) {
                    .DW_UT_compile => {}, // OK
                    else => {
                        log.errf("invalid compilation unit type: {any}", .{cu.header.unit_type});
                        return error.InvalidDWARFInfo;
                    },
                }

                // DWARF v5 changes the order of fields (switches abbrev_offset and addr_size)
                cu.header.addr_size = try dwarf.readEnum(cu.info_r, u8, types.AddressSize);
                try parseAbbrevOffset(cu);
            } else {
                try parseAbbrevOffset(cu);
                cu.header.addr_size = try dwarf.readEnum(cu.info_r, u8, types.AddressSize);
            }
        }

        fn parseAbbrevOffset(cu: *CompileUnit) !void {
            if (cu.header.is_32_bit) {
                cu.header.debug_abbrev_offset = @intCast(try read(cu.info_r, u32));
            } else {
                cu.header.debug_abbrev_offset = try read(cu.info_r, u64);
            }
        }

        test "compile unit header parse errors" {
            var arena = ArenaAllocator.init(t.allocator);
            defer arena.deinit();
            const scratch = arena.allocator();

            const fc = try file_util.Cache.init(t.allocator);
            defer fc.deinit();

            const sections = try scratch.create(dwarf.Sections);
            var cu = try scratch.create(CompileUnit);
            cu.opts = &.{
                .scratch = scratch,
                .sections = sections,
                .file_cache = fc,
            };

            {
                // readers of invalid lengths
                cu.info_r = try Reader.create(scratch, &[_]u8{});
                try t.expectError(error.InvalidDWARFInfo, Header.parse(cu));

                cu.info_r = try Reader.create(scratch, &[_]u8{ 0, 0, 0 });
                try t.expectError(error.InvalidDWARFInfo, Header.parse(cu));
            }

            {
                // 64-bit CU is not yet supported
                cu.info_r = try Reader.create(
                    scratch,
                    &[_]u8{
                        0xff, 0xff, 0xff, 0xff,
                        0xf,  0xe,  0xd,  0xc,
                        0xb,  0xa,  0x9,  0x8,
                    },
                );

                try t.expectError(error.InvalidDWARFInfo, Header.parse(cu));
            }

            {
                // invalid DWARF versions
                cu.info_r = try Reader.create(scratch, &[_]u8{
                    0, 0, 0, 0,
                    1, 0,
                });

                try t.expectError(error.InvalidDWARFVersion, Header.parse(cu));

                cu.info_r = try Reader.create(scratch, &[_]u8{
                    0, 0, 0, 0,
                    6, 0,
                });

                try t.expectError(error.InvalidDWARFVersion, Header.parse(cu));
            }

            {
                // invalid addr size
                cu.info_r = try Reader.create(
                    scratch,
                    &[_]u8{ 0, 0, 0, 0 } ++
                        &[_]u8{ 3, 0 } ++
                        &[_]u8{ 0, 0, 0, 0, 0, 0, 0, 0 } ++
                        &[_]u8{ 1, 0 }, // must be 4 or 8
                );

                try t.expectError(error.InvalidDWARFInfo, Header.parse(cu));
            }
        }
    };

    test "parse cloop compile unit header" {
        const cloop_info = getEmbeddedFile(cloop_name);

        var arena = ArenaAllocator.init(t.allocator);
        defer arena.deinit();
        const scratch = arena.allocator();

        var sections = try scratch.create(dwarf.Sections);
        sections.info.contents = cloop_info;

        const fc = try file_util.Cache.init(t.allocator);
        defer fc.deinit();

        const opts = dwarf.ParseOpts{
            .scratch = scratch,
            .sections = sections,
            .file_cache = fc,
        };

        const cu = try CompileUnit.create(&opts, 0);
        try CompileUnit.Header.parse(cu);

        try t.expectEqual(true, cu.header.is_32_bit);
        try t.expectEqual(@as(Offset, 0x327), cu.header.len);
        try t.expectEqual(@as(Offset, 0x32b), cu.header.total_len);
        try t.expectEqual(.five, cu.header.version);
        try t.expectEqual(consts.CompilationUnitHeaderType.DW_UT_compile, cu.header.unit_type);
        try t.expectEqual(@as(Offset, 0x0), cu.header.debug_abbrev_offset);
        try t.expectEqual(types.AddressSize.eight, cu.header.addr_size);
    }

    test "parse zigloop compile unit header" {
        var arena = ArenaAllocator.init(t.allocator);
        defer arena.deinit();
        const scratch = arena.allocator();

        const sections = try scratch.create(dwarf.Sections);
        sections.info.contents = getEmbeddedFile(zigloop_name);

        const fc = try file_util.Cache.init(t.allocator);
        defer fc.deinit();

        const opts = dwarf.ParseOpts{
            .scratch = scratch,
            .sections = sections,
            .file_cache = fc,
        };

        var offset: usize = 0;

        {
            // parse the first CU
            const cu = try CompileUnit.create(&opts, offset);
            try CompileUnit.Header.parse(cu);
            offset += cu.header.len + 4;

            try t.expectEqual(true, cu.header.is_32_bit);
            try t.expectEqual(@as(Offset, 0x265b4), cu.header.len);
            try t.expectEqual(.four, cu.header.version);
            try t.expectEqual(consts.CompilationUnitHeaderType.DW_UT_unknown, cu.header.unit_type);
            try t.expectEqual(@as(Offset, 0x0), cu.header.debug_abbrev_offset);
            try t.expectEqual(types.AddressSize.eight, cu.header.addr_size);
        }

        {
            // parse the second CU
            const cu = try CompileUnit.create(&opts, offset);
            try CompileUnit.Header.parse(cu);
            offset += cu.header.len + 4;

            try t.expectEqual(true, cu.header.is_32_bit);
            try t.expectEqual(@as(Offset, 0x26248), cu.header.len);
            try t.expectEqual(.four, cu.header.version);
            try t.expectEqual(consts.CompilationUnitHeaderType.DW_UT_unknown, cu.header.unit_type);
            try t.expectEqual(@as(Offset, 0x386), cu.header.debug_abbrev_offset);
            try t.expectEqual(types.AddressSize.eight, cu.header.addr_size);
        }
    }
};

/// A DIE is a reference in the .debug_info section to a particular symbol in the given
/// program (i.e. a function declaration, a variable, a block, etc.)
pub const DIE = struct {
    /// The location of the DIE in the .debug_info section
    offset: Offset,

    /// How far down the tree this DIE is. Zero indicates that this is a top-level
    /// DIE with no parent.
    depth: usize,

    /// The type of DIE (i.e. a compile unit, a function decl, a variable decl, etc.)
    tag: consts.AttributeTag,

    /// The data members belonging to this DIE
    specs: []abbrev.FormValue,
};

const cloop_name = "linux_x86-64_cloop_out_info";
const zigloop_name = "linux_x86-64_zigloop_out_info";

pub fn getEmbeddedFile(comptime name: []const u8) []const u8 {
    comptime assert(builtin.is_test);
    const contents = @embedFile("../test_files/" ++ name);
    assert(contents.len > 0);
    return contents;
}
